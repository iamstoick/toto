//! Platform socket syscalls (Wave 1, Stage 1): thin libc wrappers
//! shared by the tree-walking interpreter (blocking calls — it has
//! no scheduler to integrate with) and the runtime executor
//! (nonblocking calls + the M:N poller in `toto-runtime`).
//!
//! Unix-only (Linux/macOS): `poll`, nonblocking `fcntl`, and the
//! BSD socket API. Windows joins with the POSIX-compat decision,
//! not here — the socket API surface (§17) is platform-neutral so
//! no redesign is needed then.
//!
//! Every function here is a synchronous syscall wrapper returning
//! `Result<_, String>` with the `strerror` message on failure. No
//! Toto values cross this boundary (callers marshal); no heap
//! access, so poller and pool threads may call freely.

use std::os::raw::{c_char, c_int, c_void};

/// Last OS error as a Rust string (`strerror(errno)`).
pub fn last_error() -> String {
    let e = errno_value();
    // SAFETY: `strerror` returns a valid NUL-terminated C string for
    // any int (or a fallback message); read-only, never retained.
    unsafe {
        let p = libc_strerror(e);
        if p.is_null() {
            return format!("os error {e}");
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        let bytes = std::slice::from_raw_parts(p as *const u8, len);
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn errno_value() -> c_int {
    // SAFETY: the slot is thread-local and always valid.
    unsafe { *errno_slot() }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn __errno_location() -> *mut c_int;
}
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn __error() -> *mut c_int;
}
#[cfg(unix)]
fn errno_slot() -> *mut c_int {
    unsafe {
        #[cfg(target_os = "linux")]
        {
            __errno_location()
        }
        #[cfg(target_os = "macos")]
        {
            __error()
        }
    }
}

unsafe extern "C" {
    #[link_name = "strerror"]
    fn libc_strerror(errnum: c_int) -> *mut c_char;
    #[link_name = "socket"]
    fn libc_socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    #[link_name = "bind"]
    fn libc_bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    #[link_name = "listen"]
    fn libc_listen(fd: c_int, backlog: c_int) -> c_int;
    #[link_name = "accept"]
    fn libc_accept(fd: c_int, addr: *mut c_void, len: *mut u32) -> c_int;
    #[link_name = "connect"]
    fn libc_connect(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    #[link_name = "send"]
    fn libc_send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> isize;
    #[link_name = "recv"]
    fn libc_recv(fd: c_int, buf: *mut c_void, len: usize, flags: c_int) -> isize;
    #[link_name = "close"]
    fn libc_close(fd: c_int) -> c_int;
    #[link_name = "setsockopt"]
    fn libc_setsockopt(fd: c_int, level: c_int, name: c_int, val: *const c_void, len: u32)
    -> c_int;
    #[link_name = "fcntl"]
    fn libc_fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int;
    #[link_name = "getsockopt"]
    fn libc_getsockopt(
        fd: c_int,
        level: c_int,
        name: c_int,
        val: *mut c_void,
        len: *mut u32,
    ) -> c_int;
}

// Symbol shim: the real symbols are `socket`, `bind`, … — declared
// under `libc_` prefixes (via `#[link_name]`) because `close`/`read`/
// `open` collide with Toto builtins in documentation-adjacent code,
// and to keep every foreign symbol in this file visibly namespaced.

/// Parse a numeric IP literal (v4 dotted-quad or v6) to a wire
/// sockaddr via `getaddrinfo` with `AI_NUMERICHOST` — parsing only,
/// never DNS, never blocking. Returns the raw sockaddr bytes + len.
pub fn parse_numeric_host(host: &str, port: u16) -> Result<(Vec<u8>, u32), String> {
    use std::ffi::CString;
    // Net debug trace (see sched op_net).
    if std::env::var_os("TOTO_NET_DEBUG").is_some() {
        eprintln!("T-NET parse {host:?}");
    }
    let h = CString::new(host).map_err(|_| format!("invalid IP literal {host:?}"))?;
    let svc = port.to_string();
    let s = CString::new(svc).unwrap();
    // Hints: AF_UNSPEC, SOCK_STREAM, AI_NUMERICHOST=0x4 (both platforms).
    #[repr(C)]
    struct Hints {
        flags: c_int,
        family: c_int,
        socktype: c_int,
        protocol: c_int,
        _rest: [u8; 32],
    }
    let hints = Hints {
        flags: 0x4,
        family: 0,
        socktype: 1,
        protocol: 0,
        _rest: [0; 32],
    };
    let mut res: *mut c_void = std::ptr::null_mut();
    // SAFETY: strings + hints outlive the call; result freed below.
    let r = unsafe {
        libc_getaddrinfo(
            h.as_ptr(),
            s.as_ptr(),
            &hints as *const Hints as *const c_void,
            &mut res as *mut *mut c_void,
        )
    };
    if r != 0 {
        return Err(format!("not a numeric IP address: {host:?}"));
    }
    // First result only (loopback tests use one address).
    // Layout: ai_addrlen u32@16, ai_addr *@24 (64-bit Unix).
    // SAFETY: `res` points at a live result list; freed below.
    let (bytes, len) = unsafe {
        if res.is_null() {
            libc_freeaddrinfo(res);
            return Err(format!("could not parse IP address {host:?}"));
        }
        let len = *(res as *const u32).add(4) as u32;
        let addr = *(res as *const *const c_void).add(3);
        if addr.is_null() || len == 0 || len > 64 {
            libc_freeaddrinfo(res);
            return Err(format!("could not parse IP address {host:?}"));
        }
        let bytes = std::slice::from_raw_parts(addr as *const u8, len as usize).to_vec();
        (bytes, len)
    };
    unsafe { libc_freeaddrinfo(res) };
    Ok((bytes, len))
}

/// Blocking `connect` to a numeric IP literal (interpreter path).
pub fn connect_ip_blocking(ip: &str, port: u16) -> Result<c_int, String> {
    let (addr, len) = parse_numeric_host(ip, port)?;
    let fd = socket_tcp()?;
    // SAFETY: `addr` outlives the call; the kernel copies it in.
    let r = unsafe { libc_connect(fd, addr.as_ptr() as *const c_void, len) };
    if r == 0 {
        Ok(fd)
    } else {
        let e = last_error();
        let _ = close(fd);
        Err(e)
    }
}

/// Bind + listen on loopback (shared by TCP `listen_loopback`).
fn bind_loopback_socket(ty: c_int, port: u16) -> Result<c_int, String> {
    let fd = socket_of(ty)?;
    if let Err(e) = bind_loopback(fd, port) {
        let _ = close(fd);
        return Err(e);
    }
    Ok(fd)
}

/// `socket` + `bind(127.0.0.1:port)` + `listen` (interpreter path;
/// the executor inlines the same steps before its first park).
pub fn listen_loopback(port: u16) -> Result<c_int, String> {
    let fd = bind_loopback_socket(1, port)?;
    if let Err(e) = listen(fd) {
        let _ = close(fd);
        return Err(e);
    }
    Ok(fd)
}

/// `socket` + `bind(127.0.0.1:port)` for UDP (no listen).
pub fn bind_udp_loopback(port: u16) -> Result<c_int, String> {
    bind_loopback_socket(2, port)
}

/// Render a peer sockaddr (from `recvfrom`) as a numeric host string.
/// Hand-parsed (no `getnameinfo` — its numeric-only flags differ by
/// platform); same layout math as `resolve_blocking`.
pub fn sockaddr_to_host(addr: &[u8]) -> Option<(String, u16)> {
    if addr.len() < 8 {
        return None;
    }
    // Family position is platform-dependent: BSD sockets (macOS)
    // carry a `sin_len` byte first, so family lives at [1];
    // Linux packs the u16 family at [0..2]. Port (big-endian)
    // and address offsets coincide on both.
    #[cfg(target_vendor = "apple")]
    let fam = addr[1] as u16;
    #[cfg(not(target_vendor = "apple"))]
    let fam = u16::from_ne_bytes([addr[0], addr[1]]);
    let port = u16::from_be_bytes([addr[2], addr[3]]);
    if fam == 2 && addr.len() >= 8 {
        return Some((
            format!("{}.{}.{}.{}", addr[4], addr[5], addr[6], addr[7]),
            port,
        ));
    }
    if (fam == 10 || fam == 30) && addr.len() >= 24 {
        return Some((render_ipv6(&addr[8..24]), port));
    }
    None
}

/// Blocking UDP `sendto` to a numeric IP literal (interpreter path).
pub fn udp_send_to(fd: c_int, buf: &[u8], ip: &str, port: u16) -> Result<usize, String> {
    let (addr, len) = parse_numeric_host(ip, port)?;
    // SAFETY: `buf` and `addr` outlive the call.
    let r = unsafe {
        libc_sendto(
            fd,
            if buf.is_empty() {
                std::ptr::null()
            } else {
                buf.as_ptr()
            } as *const c_void,
            buf.len(),
            0,
            addr.as_ptr() as *const c_void,
            len,
        )
    };
    if r < 0 {
        Err(last_error())
    } else {
        Ok(r as usize)
    }
}

unsafe extern "C" {
    #[link_name = "sendto"]
    fn libc_sendto(
        fd: c_int,
        buf: *const c_void,
        len: usize,
        flags: c_int,
        addr: *const c_void,
        addrlen: u32,
    ) -> isize;
    #[link_name = "recvfrom"]
    fn libc_recvfrom(
        fd: c_int,
        buf: *mut c_void,
        len: usize,
        flags: c_int,
        addr: *mut c_void,
        addrlen: *mut u32,
    ) -> isize;
}

/// Blocking UDP `recvfrom` (interpreter path): payload plus the
/// numeric peer (host string, port).
pub fn udp_recv_from(fd: c_int, max: usize) -> Result<(Vec<u8>, String, u16), String> {
    match udp_recv_from_nb(fd, max) {
        UdpRecvOutcome::Got(bytes, host, port) => Ok((bytes, host, port)),
        UdpRecvOutcome::WouldBlock => Err("would block".to_string()),
        UdpRecvOutcome::Failed(e) => Err(e),
    }
}

/// One nonblocking `recvfrom`: payload + numeric peer, WouldBlock
/// when no datagram waits, or a loud failure. Empty datagrams are
/// real data (`Got` with an empty vec), never shutdown — UDP has
/// no orderly-close signal.
pub enum UdpRecvOutcome {
    Got(Vec<u8>, String, u16),
    WouldBlock,
    Failed(String),
}

pub fn udp_recv_from_nb(fd: c_int, max: usize) -> UdpRecvOutcome {
    let mut buf = vec![0u8; max.max(1)];
    let mut addr = [0u8; 64];
    let mut addrlen: u32 = 64;
    // SAFETY: all buffers owned and outlive the call.
    let r = unsafe {
        libc_recvfrom(
            fd,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            0,
            addr.as_mut_ptr() as *mut c_void,
            &mut addrlen as *mut u32,
        )
    };
    if r < 0 {
        return match errno_value() {
            11 | 35 => UdpRecvOutcome::WouldBlock,
            _ => UdpRecvOutcome::Failed(last_error()),
        };
    }
    buf.truncate(r as usize);
    let addrlen = (addrlen as usize).min(64);
    match sockaddr_to_host(&addr[..addrlen]) {
        Some((host, port)) => UdpRecvOutcome::Got(buf, host, port),
        None => UdpRecvOutcome::Failed("could not parse peer address".to_string()),
    }
}

/// One nonblocking UDP `sendto` to a numeric IP (never blocks DNS:
/// `AI_NUMERICHOST` parsing only — use `net_resolve` first for
/// hostnames). Reuses the `SendOutcome` discipline.
pub fn udp_send_to_nb(fd: c_int, buf: &[u8], ip: &str, port: u16) -> SendOutcome {
    let (addr, len) = match parse_numeric_host(ip, port) {
        Ok(a) => a,
        Err(e) => return SendOutcome::Failed(e),
    };
    // SAFETY: `buf` and `addr` outlive the call.
    let r = unsafe {
        libc_sendto(
            fd,
            if buf.is_empty() {
                std::ptr::null()
            } else {
                buf.as_ptr()
            } as *const c_void,
            buf.len(),
            0,
            addr.as_ptr() as *const c_void,
            len,
        )
    };
    if r < 0 {
        match errno_value() {
            11 | 35 => SendOutcome::WouldBlock,
            _ => SendOutcome::Failed(last_error()),
        }
    } else {
        SendOutcome::Wrote(r as usize)
    }
}

/// Last-sender table for UDP sockets: fd → (host, port), set on
/// every successful `recvfrom`, cleared by `close`. Process-global
/// (an fd names a process-global socket) behind a mutex — the
/// poller, pool, workers, and interpreter all share it, and it
/// never touches the GC heap. Entries for closed fds are dropped
/// eagerly so a recycled fd number never reports a stale peer.
static UDP_PEERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<c_int, (String, u16)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub fn record_peer(fd: c_int, host: String, port: u16) {
    UDP_PEERS
        .lock()
        .expect("peer table lock")
        .insert(fd, (host, port));
}

pub fn lookup_peer(fd: c_int) -> Option<(String, u16)> {
    UDP_PEERS.lock().expect("peer table lock").get(&fd).cloned()
}

pub fn forget_peer(fd: c_int) {
    UDP_PEERS.lock().expect("peer table lock").remove(&fd);
}

/// A self-pipe for poller wakeups: writing one byte wakes a `poll`
/// blocked on the read end. Both ends nonblocking (a full pipe must
/// never stall the writer; the reader drains to `EAGAIN`). Used to
/// deliver new registrations, `close` sweeps, and shutdown.
pub fn self_pipe() -> (c_int, c_int) {
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` is a valid 2-int out-array.
    let r = unsafe { libc_pipe(fds.as_mut_ptr()) };
    if r < 0 {
        panic!("net poller self-pipe failed: {}", last_error());
    }
    let _ = set_nonblocking(fds[0]);
    let _ = set_nonblocking(fds[1]);
    (fds[0], fds[1])
}

/// Drain a self-pipe read end (until `EAGAIN`).
pub fn drain_pipe(fd: c_int) {
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: `buf` is owned and outlives the call.
        let r = unsafe { libc_read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if r <= 0 {
            return;
        }
    }
}

/// Write one wakeup byte; a full pipe is fine (level-triggered: the
/// byte already pending is wakeup enough).
pub fn poke_pipe(fd: c_int) {
    let one = [1u8; 1];
    // SAFETY: `one` outlives the call; O_NONBLOCK means this never stalls.
    let _ = unsafe { libc_write(fd, one.as_ptr() as *const c_void, 1) };
}

unsafe extern "C" {
    #[link_name = "pipe"]
    fn libc_pipe(fds: *mut c_int) -> c_int;
    #[link_name = "read"]
    fn libc_read(fd: c_int, buf: *mut c_void, len: usize) -> isize;
    #[link_name = "write"]
    fn libc_write(fd: c_int, buf: *const c_void, len: usize) -> isize;
}

/// An IPv4 loopback socket address (`127.0.0.1:port`) in the
/// kernel's own layout, via `getaddrinfo` (Wave 1 macOS fix: a
/// hand-rolled Linux `sockaddr_in` lacks the BSD `sin_len` byte,
/// so `bind`/`connect` failed on macOS. Asking libc removes the
/// per-platform layout from our code entirely).
pub fn sockaddr_loopback_v4(port: u16) -> Result<(Vec<u8>, u32), String> {
    parse_numeric_host("127.0.0.1", port)
}

/// Create a TCP (`SOCK_STREAM`) or UDP (`SOCK_DGRAM`) socket.
pub fn socket_tcp() -> Result<c_int, String> {
    socket_of(1)
}

/// Create a UDP socket.
pub fn socket_udp() -> Result<c_int, String> {
    socket_of(2)
}

fn socket_of(ty: c_int) -> Result<c_int, String> {
    // SAFETY: plain integers; returns a fresh fd or -1 with errno set.
    let fd = unsafe { libc_socket(2, ty, 0) };
    if fd < 0 { Err(last_error()) } else { Ok(fd) }
}

/// Mark a socket nonblocking (executor path; the interpreter uses
/// blocking sockets and never calls this).
pub fn set_nonblocking(fd: c_int) -> Result<(), String> {
    // F_GETFL=3, F_SETFL=4, O_NONBLOCK=0x800 (both Linux and macOS).
    let flags = unsafe { libc_fcntl(fd, 3, 0) };
    if flags < 0 {
        return Err(last_error());
    }
    // SAFETY: OR-ing O_NONBLOCK into the retrieved flags.
    let r = unsafe { libc_fcntl(fd, 4, flags | 0x800) };
    if r < 0 { Err(last_error()) } else { Ok(()) }
}

/// Bind to loopback with `SO_REUSEADDR` (tests re-bind the same
/// port across runs and suites).
pub fn bind_loopback(fd: c_int, port: u16) -> Result<(), String> {
    let one: c_int = 1;
    // SOL_SOCKET=1, SO_REUSEADDR=2 on both Linux and macOS.
    // SAFETY: plain ints and a pointer to a stack int.
    let r = unsafe { libc_setsockopt(fd, 1, 2, &one as *const c_int as *const c_void, 4) };
    if r < 0 {
        return Err(last_error());
    }
    let (addr, len) = sockaddr_loopback_v4(port)?;
    // SAFETY: `addr` outlives the call; the kernel copies it in.
    let r = unsafe { libc_bind(fd, addr.as_ptr() as *const c_void, len) };
    if r < 0 { Err(last_error()) } else { Ok(()) }
}

/// `listen` with a modest backlog (test/small-service scale).
pub fn listen(fd: c_int) -> Result<(), String> {
    // SAFETY: plain ints.
    let r = unsafe { libc_listen(fd, 16) };
    if r < 0 { Err(last_error()) } else { Ok(()) }
}

/// Blocking `accept` (interpreter path).
pub fn accept_blocking(listener: c_int) -> Result<c_int, String> {
    // SAFETY: NULL addr/len asks the kernel to skip the peer address.
    let fd = unsafe { libc_accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
    if fd < 0 { Err(last_error()) } else { Ok(fd) }
}

/// Nonblocking `accept`: `Ok(fd)`, or `Err(WouldBlock)` when no
/// connection is waiting (`EAGAIN`/`EWOULDBLOCK`), or `Err(Os(msg))`
/// for real failures.
pub enum AcceptOutcome {
    Accepted(c_int),
    WouldBlock,
    Failed(String),
}

pub fn accept_nonblocking(listener: c_int) -> AcceptOutcome {
    // SAFETY: NULL addr/len; nonblocking fd, so this never stalls.
    let fd = unsafe { libc_accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
    if fd >= 0 {
        AcceptOutcome::Accepted(fd)
    } else {
        match errno_value() {
            // EAGAIN=11 (Linux) / 35 (macOS); EWOULDBLOCK aliases it.
            11 | 35 => AcceptOutcome::WouldBlock,
            _ => AcceptOutcome::Failed(last_error()),
        }
    }
}

/// Start a nonblocking `connect` to a numeric IP: immediate success,
/// `InProgress` when the handshake is in flight (`EINPROGRESS` = 115
/// on Linux / 36 on macOS — poll for writability, then check
/// [`so_error`]), or `Failed`. The fd stays owned by the caller on
/// every outcome (close it on `Failed`).
pub enum ConnectOutcome {
    Connected(c_int),
    InProgress(c_int),
    Failed(String),
}

pub fn connect_start(fd: c_int, ip: &str, port: u16) -> ConnectOutcome {
    // Net debug trace (see sched op_net).
    if std::env::var_os("TOTO_NET_DEBUG").is_some() {
        eprintln!("T-NET connect_start {ip:?}");
    }
    let (addr, len) = match parse_numeric_host(ip, port) {
        Ok(a) => a,
        Err(e) => return ConnectOutcome::Failed(e),
    };
    // SAFETY: `addr` outlives the call; the kernel copies it in.
    let r = unsafe { libc_connect(fd, addr.as_ptr() as *const c_void, len) };
    if r == 0 {
        ConnectOutcome::Connected(fd)
    } else {
        match errno_value() {
            115 | 36 => ConnectOutcome::InProgress(fd),
            _ => ConnectOutcome::Failed(last_error()),
        }
    }
}

/// `SO_ERROR` after a writable poll on a connecting socket:
/// 0 means connected, otherwise the deferred error string.
pub fn so_error(fd: c_int) -> Result<(), String> {
    let mut err: c_int = 0;
    let mut len: u32 = 4;
    // SOL_SOCKET=1, SO_ERROR=4 on both Linux and macOS.
    // SAFETY: stack ints outlive the call.
    let r = unsafe {
        libc_getsockopt(
            fd,
            1,
            4,
            &mut err as *mut c_int as *mut c_void,
            &mut len as *mut u32,
        )
    };
    if r < 0 {
        return Err(last_error());
    }
    if err == 0 {
        Ok(())
    } else {
        // SAFETY: same contract as `last_error`.
        unsafe {
            let p = libc_strerror(err);
            if p.is_null() {
                return Err(format!("os error {err}"));
            }
            let mut n = 0usize;
            while *p.add(n) != 0 {
                n += 1;
            }
            let bytes = std::slice::from_raw_parts(p as *const u8, n);
            Err(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

/// One nonblocking `send`. Returns bytes written, `WouldBlock` when
/// the buffer is full, or `Failed` on real errors. Short writes are
/// normal — the caller loops (see `net_send_all`).
pub enum SendOutcome {
    Wrote(usize),
    WouldBlock,
    Failed(String),
    Closed,
}

/// One nonblocking `recv` (up to `max` bytes). `Closed` is orderly
/// shutdown (0 return), NOT an error.
pub enum RecvOutcome {
    Got(Vec<u8>),
    Closed,
    WouldBlock,
    Failed(String),
}

pub fn send_nonblocking(fd: c_int, buf: &[u8]) -> SendOutcome {
    if buf.is_empty() {
        return SendOutcome::Wrote(0);
    }
    // SAFETY: `buf` outlives the call; MSG_NOSIGNAL=0x4000 avoids
    // SIGPIPE on Linux (0 elsewhere — macOS has no MSG_NOSIGNAL
    // and ignores unknown flags here; SIGPIPE on macOS pipes is
    // out of scope for loopback tests).
    #[cfg(target_os = "linux")]
    let flags = 0x4000;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let r = unsafe { libc_send(fd, buf.as_ptr() as *const c_void, buf.len(), flags) };
    if r >= 0 {
        return SendOutcome::Wrote(r as usize);
    }
    match errno_value() {
        11 | 35 => SendOutcome::WouldBlock,
        // EPIPE=32: peer closed. Report as clean closure, not an error.
        32 => SendOutcome::Closed,
        _ => SendOutcome::Failed(last_error()),
    }
}

pub fn recv_nonblocking(fd: c_int, max: usize) -> RecvOutcome {
    let mut buf = vec![0u8; max.max(1)];
    // SAFETY: `buf` is owned, sized, and outlives the call.
    let r = unsafe { libc_recv(fd, buf.as_mut_ptr() as *mut c_void, buf.len(), 0) };
    if r > 0 {
        buf.truncate(r as usize);
        RecvOutcome::Got(buf)
    } else if r == 0 {
        RecvOutcome::Closed
    } else {
        match errno_value() {
            11 | 35 => RecvOutcome::WouldBlock,
            _ => RecvOutcome::Failed(last_error()),
        }
    }
}

/// Blocking `send_all` (interpreter path): loops until everything is
/// written or an error/close occurs. Returns bytes written.
pub fn send_all_blocking(fd: c_int, mut buf: &[u8]) -> Result<usize, String> {
    let mut wrote = 0usize;
    while !buf.is_empty() {
        // SAFETY: `buf` outlives each call.
        #[cfg(target_os = "linux")]
        let flags = 0x4000;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let r = unsafe { libc_send(fd, buf.as_ptr() as *const c_void, buf.len(), flags) };
        if r < 0 {
            let e = errno_value();
            if e == 11 || e == 35 {
                // Blocking socket spuriously not ready; retry.
                continue;
            }
            if e == 32 {
                return Err("connection closed by peer".to_string());
            }
            return Err(last_error());
        }
        let n = r as usize;
        wrote += n;
        buf = &buf[n..];
    }
    Ok(wrote)
}

/// Blocking `recv` up to `max` bytes (interpreter path). Empty vec
/// is orderly shutdown, not an error.
pub fn recv_blocking(fd: c_int, max: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; max.max(1)];
    // SAFETY: `buf` is owned, sized, and outlives the call.
    let r = unsafe { libc_recv(fd, buf.as_mut_ptr() as *mut c_void, buf.len(), 0) };
    if r > 0 {
        buf.truncate(r as usize);
        Ok(buf)
    } else if r == 0 {
        Ok(Vec::new())
    } else {
        Err(last_error())
    }
}

/// Blocking `connect` to loopback (interpreter path).
pub fn connect_blocking(port: u16) -> Result<c_int, String> {
    let fd = socket_tcp()?;
    let (addr, len) = sockaddr_loopback_v4(port)?;
    // SAFETY: `addr` outlives the call; the kernel copies it in.
    let r = unsafe { libc_connect(fd, addr.as_ptr() as *const c_void, len) };
    if r == 0 {
        Ok(fd)
    } else {
        let e = last_error();
        let _ = close(fd);
        Err(e)
    }
}

/// Close a socket. `close` on a loopback socket never meaningfully
/// blocks (no `SO_LINGER` set anywhere here).
pub fn close(fd: c_int) -> Result<(), String> {
    // SAFETY: plain int; EBADF and friends surface as strings.
    let r = unsafe { libc_close(fd) };
    if r < 0 { Err(last_error()) } else { Ok(()) }
}

/// Resolve a hostname to IP strings via blocking `getaddrinfo`
/// (pool thread or interpreter — never a worker).
pub fn resolve_blocking(host: &str) -> Result<Vec<String>, String> {
    use std::ffi::CString;
    // Net debug trace (see sched op_net): enabled only with
    // TOTO_NET_DEBUG set.
    if std::env::var_os("TOTO_NET_DEBUG").is_some() {
        eprintln!("T-NET resolve start {host:?}");
    }
    let h = CString::new(host).map_err(|_| format!("invalid hostname {host:?}"))?;
    // SAFETY: `h` outlives the call; NULL hints/service select
    // defaults; the result list is freed below before return.
    let mut res: *mut c_void = std::ptr::null_mut();
    let r = unsafe {
        libc_getaddrinfo(
            h.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            &mut res as *mut *mut c_void,
        )
    };
    if r != 0 {
        return Err(gai_error(r));
    }
    let mut out = Vec::new();
    let mut cur = res;
    while !cur.is_null() {
        // ai_family at offset 0 (int), ai_addr/ai_addrlen follow the
        // libc layout; read the address bytes directly instead of
        // trusting struct offsets beyond family + pointer.
        //
        // Layout (both Linux and macOS, 64-bit): ai_flags int@0,
        // ai_family int@4, ai_socktype int@8, ai_protocol int@12,
        // ai_addrlen u32/socklen_t@16, ai_addr *@24, ai_next *@32.
        // SAFETY: `cur` points to a live addrinfo node owned by the
        // result list until `freeaddrinfo` below.
        let fam = unsafe { *(cur as *const c_int).add(1) };
        let addr_ptr = unsafe { *(cur as *const *const c_void).add(3) };
        if !addr_ptr.is_null() {
            if fam == 2 {
                // AF_INET: sin_port[2..4], sin_addr[4..8].
                // SAFETY: IPv4 sockaddr is 16 bytes; reading 8 is safe.
                let b = unsafe { std::slice::from_raw_parts(addr_ptr as *const u8, 8) };
                out.push(format!("{}.{}.{}.{}", b[4], b[5], b[6], b[7]));
            } else if fam == 10 || fam == 30 {
                // AF_INET6 is 10 (Linux) / 30 (macOS): 16 address
                // bytes at offset 8. Rendered compressed below.
                // SAFETY: IPv6 sockaddr is 28 bytes; reading 24 is safe.
                let b = unsafe { std::slice::from_raw_parts(addr_ptr as *const u8, 24) };
                out.push(render_ipv6(&b[8..24]));
            }
        }
        // SAFETY: ai_next at offset 40 on 64-bit Unix (flags,
        // family, socktype, protocol, addrlen, addr *@24,
        // canonname *@32, next *@40).
        cur = unsafe { *(cur as *const *mut c_void).add(5) };
    }
    // SAFETY: frees the list from `getaddrinfo`.
    unsafe { libc_freeaddrinfo(res) };
    if std::env::var_os("TOTO_NET_DEBUG").is_some() {
        eprintln!("T-NET resolve done count={}", out.len());
    }
    if out.is_empty() {
        Err(format!("could not resolve {host:?}"))
    } else {
        Ok(out)
    }
}

/// Minimal IPv6 rendering (full groups, no `::` compression — tests
/// pin `::1` for localhost via prefix matching, not this string).
fn render_ipv6(b: &[u8]) -> String {
    let mut g = [0u16; 8];
    let (chunks, _) = b.as_chunks::<2>();
    for (i, chunk) in chunks.iter().enumerate() {
        g[i] = u16::from_be_bytes(*chunk);
    }
    g.iter()
        .map(|x| format!("{x:x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn gai_error(code: c_int) -> String {
    // SAFETY: `gai_strerror` returns a static NUL-terminated string.
    unsafe {
        let p = libc_gai_strerror(code);
        if p.is_null() {
            return format!("getaddrinfo failed ({code})");
        }
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        let bytes = std::slice::from_raw_parts(p as *const u8, n);
        String::from_utf8_lossy(bytes).into_owned()
    }
}

unsafe extern "C" {
    #[link_name = "getaddrinfo"]
    fn libc_getaddrinfo(
        host: *const c_char,
        service: *const c_char,
        hints: *const c_void,
        res: *mut *mut c_void,
    ) -> c_int;
    #[link_name = "freeaddrinfo"]
    fn libc_freeaddrinfo(res: *mut c_void);
    #[link_name = "gai_strerror"]
    fn libc_gai_strerror(code: c_int) -> *mut c_char;
}

/// One `poll()` wait over `(fd, want_read, want_write)` triples.
/// Returns the indexes that are ready. `timeout_ms < 0` waits
/// indefinitely (the poller thread has nothing else to do).
/// `POLLIN=0x1`, `POLLOUT=0x4` on both Linux and macOS.
pub fn poll_wait(fds: &[(c_int, bool, bool)], timeout_ms: c_int) -> Result<Vec<usize>, String> {
    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: i16,
        revents: i16,
    }
    unsafe extern "C" {
        #[link_name = "poll"]
        fn libc_poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
    }
    let mut pfds: Vec<PollFd> = fds
        .iter()
        .map(|(fd, r, w)| PollFd {
            fd: *fd,
            events: (*r as i16) | ((*w as i16) * 0x4),
            revents: 0,
        })
        .collect();
    // SAFETY: `pfds` is owned and outlives the call; the kernel
    // writes only `revents`.
    let r = unsafe { libc_poll(pfds.as_mut_ptr(), pfds.len() as u64, timeout_ms) };
    if r < 0 {
        return Err(last_error());
    }
    Ok(pfds
        .iter()
        .enumerate()
        .filter(|(_, p)| p.revents != 0)
        .map(|(i, _)| i)
        .collect())
}
