// `dblib` — database drivers via mature system client libraries
// (Enterprise Wave 3, Stage 1).
//
// Toto calls these through `toto_rt_db_*` foreign functions
// (declared in `gzv.std/db`, allowlisted in `ffi.rs`, refused by
// the JIT like the net/crypto/TLS surface). Every query runs on
// the dedicated blocking pool (never inline on a worker, never on
// the poller — a checkout or a query blocks; see the Stage 1
// blocking-I/O policy in the Wave 3 report). The interpreter calls
// these inline (its blocking-call discipline, same as TLS).
//
// FFI-first, no Toto-implemented drivers anywhere: SQLite binds
// the system libsqlite3 (unix-wide), Postgres binds libpq and
// MySQL binds libmysqlclient (both Linux-only — macOS ships
// neither as a system library, and the modules are
// `cfg(target_os = "linux")` to match). Only long-stable,
// opaque-pointer-only APIs are used. Structs-by-value, dlopen,
// and binding generation stay deferred per the Waves 0–2
// discipline. Nothing here implements a wire protocol, a query
// planner, or storage logic.
//
// Pooling (the documented policy): one process-global pool per
// (driver, DSN) behind a Mutex+Condvar (the SESSIONS precedent).
// A pool job checks out exactly one connection, runs exactly one
// statement, and returns the handle — handles never leave the
// pool job, so one-task-at-a-time holds by construction. Waiting
// for a connection uses a Condvar with the configured checkout
// timeout (pool exhaustion is a loud `Err`, never a silent stall).
// Pool threads never touch the GC heap (plain `DbCell` only);
// the scheduler drain converts to heap values where the heap is
// available.
//
// Value model: `DbCell` (kind-tagged plain data). Drivers decode
// int/float/bool/text by native type; anything else arrives as
// text (documented per driver). No blob in v1.

#[cfg(unix)]
pub use inner::*;
#[cfg(unix)]
mod inner {
    use std::collections::HashMap;
    use std::sync::{Condvar, LazyLock, Mutex};
    use std::time::{Duration, Instant};

    /// Kind tags for `DbCell` (match the `DbVal.kind` contract in
    /// `gzv.std/db` and the exec/db drain mapping exactly).
    pub const CELL_NULL: u8 = 0;
    pub const CELL_INT: u8 = 1;
    pub const CELL_FLOAT: u8 = 2;
    pub const CELL_TEXT: u8 = 3;
    pub const CELL_BOOL: u8 = 4;

    /// One plain database value. Pool-safe: no heap refs, no
    /// pointers — pool threads convert native values into these,
    /// and the scheduler drain converts these into GC values.
    #[derive(Clone, Debug)]
    pub struct DbCell {
        pub kind: u8,
        pub i: i64,
        pub f: f64,
        pub s: String,
    }

    impl DbCell {
        pub fn null() -> Self {
            Self {
                kind: CELL_NULL,
                i: 0,
                f: 0.0,
                s: String::new(),
            }
        }
        pub fn int(i: i64) -> Self {
            Self {
                kind: CELL_INT,
                i,
                f: 0.0,
                s: String::new(),
            }
        }
        pub fn float(f: f64) -> Self {
            Self {
                kind: CELL_FLOAT,
                i: 0,
                f,
                s: String::new(),
            }
        }
        pub fn text(s: String) -> Self {
            Self {
                kind: CELL_TEXT,
                i: 0,
                f: 0.0,
                s,
            }
        }
        pub fn bool_(b: bool) -> Self {
            Self {
                kind: CELL_BOOL,
                i: if b { 1 } else { 0 },
                f: 0.0,
                s: String::new(),
            }
        }
    }

    /// Supported drivers (the DSN scheme selects exactly one).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum Driver {
        Sqlite,
        #[cfg(target_os = "linux")]
        Postgres,
        #[cfg(target_os = "linux")]
        Mysql,
    }

    /// Parsed DSN: `sqlite:path=...` / `sqlite::memory:`,
    /// `postgres:k=v ...`, `mysql:k=v ...`.
    pub struct Dsn {
        pub driver: Driver,
        /// Opaque remainder (path or `k=v` pairs); each driver
        /// parses its own remainder.
        pub rest: String,
    }

    /// Parse `scheme:remainder` (the scheme names the driver).
    /// Unknown schemes and empty remainders are loud `Err`s.
    pub fn parse_dsn(dsn: &str) -> Result<Dsn, String> {
        let (scheme, rest) = dsn
            .split_once(':')
            .ok_or_else(|| format!("bad DSN (want scheme:...): {dsn:?}"))?;
        let rest = rest.to_string();
        if rest.is_empty() {
            return Err(format!("bad DSN (empty remainder): {dsn:?}"));
        }
        let driver = match scheme {
            "sqlite" => Driver::Sqlite,
            #[cfg(target_os = "linux")]
            "postgres" => Driver::Postgres,
            #[cfg(target_os = "linux")]
            "mysql" => Driver::Mysql,
            _ => {
                return Err(format!(
                    "unknown database driver {scheme:?} (want sqlite/postgres/mysql)"
                ));
            }
        };
        Ok(Dsn { driver, rest })
    }

    /// Split `k=v` pairs on ASCII whitespace (`v` holds no spaces;
    /// passwords with spaces are a documented v1 limitation — use
    /// the config layer, Stage 4, which quotes properly).
    pub fn kv_pairs(rest: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for part in rest.split_ascii_whitespace() {
            if let Some((k, v)) = part.split_once('=') {
                out.insert(k.to_string(), v.to_string());
            }
        }
        out
    }

    /// Translate Toto `?` placeholders for Postgres `$n` (1-based).
    /// `?` inside single/double-quoted literals and inside line or
    /// block comments is NOT a placeholder. Small, tested, and
    /// documented — the only string surgery in this module (NOT
    /// escaping: values still bind server-side via `PQexecParams`).
    /// Public so the translation is unit-testable on its own.
    pub fn translate_pg(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len() + 8);
        let mut n = 0u32;
        let bytes = sql.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i] as char;
            match c {
                '\'' | '"' => {
                    // Quoted literal: copy through the closer,
                    // honoring backslash and doubled-quote escapes.
                    out.push(c);
                    i += 1;
                    while i < bytes.len() {
                        let d = bytes[i] as char;
                        out.push(d);
                        i += 1;
                        if d == '\\' && i < bytes.len() {
                            out.push(bytes[i] as char);
                            i += 1;
                        } else if d == c {
                            if i < bytes.len() && bytes[i] as char == c {
                                out.push(c);
                                i += 1;
                            } else {
                                break;
                            }
                        }
                    }
                }
                '-' if i + 1 < bytes.len() && bytes[i + 1] as char == '-' => {
                    while i < bytes.len() && bytes[i] as char != '\n' {
                        out.push(bytes[i] as char);
                        i += 1;
                    }
                }
                '/' if i + 1 < bytes.len() && bytes[i + 1] as char == '*' => {
                    out.push('/');
                    out.push('*');
                    i += 2;
                    while i + 1 < bytes.len()
                        && !(bytes[i] as char == '*' && bytes[i + 1] as char == '/')
                    {
                        out.push(bytes[i] as char);
                        i += 1;
                    }
                    if i + 1 < bytes.len() {
                        out.push('*');
                        out.push('/');
                        i += 2;
                    }
                }
                '?' => {
                    n += 1;
                    out.push('$');
                    out.push_str(&n.to_string());
                    i += 1;
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    /// Split Toto SQL on `?` placeholders outside literals/comments
    /// (the `translate_pg` walk, but collecting literal segments).
    /// The caller interleaves the segments with escaped values.
    /// Used only by the MySQL path (whose escaping is the mature
    /// library's own `mysql_real_escape_string`, never Toto logic).
    pub fn split_placeholders(sql: &str) -> Vec<String> {
        let mut segs = Vec::new();
        let mut cur = String::new();
        let bytes = sql.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i] as char;
            match c {
                '\'' | '"' => {
                    cur.push(c);
                    i += 1;
                    while i < bytes.len() {
                        let d = bytes[i] as char;
                        cur.push(d);
                        i += 1;
                        if d == '\\' && i < bytes.len() {
                            cur.push(bytes[i] as char);
                            i += 1;
                        } else if d == c {
                            if i < bytes.len() && bytes[i] as char == c {
                                cur.push(c);
                                i += 1;
                            } else {
                                break;
                            }
                        }
                    }
                }
                '-' if i + 1 < bytes.len() && bytes[i + 1] as char == '-' => {
                    while i < bytes.len() && bytes[i] as char != '\n' {
                        cur.push(bytes[i] as char);
                        i += 1;
                    }
                }
                '/' if i + 1 < bytes.len() && bytes[i + 1] as char == '*' => {
                    cur.push('/');
                    cur.push('*');
                    i += 2;
                    while i + 1 < bytes.len()
                        && !(bytes[i] as char == '*' && bytes[i + 1] as char == '/')
                    {
                        cur.push(bytes[i] as char);
                        i += 1;
                    }
                    if i + 1 < bytes.len() {
                        cur.push('*');
                        cur.push('/');
                        i += 2;
                    }
                }
                '?' => {
                    segs.push(std::mem::take(&mut cur));
                    i += 1;
                }
                _ => {
                    cur.push(c);
                    i += 1;
                }
            }
        }
        segs.push(cur);
        segs
    }

    // ------------------------------------------------------------------
    // Connection pool (process-global, per (driver, DSN)).
    // ------------------------------------------------------------------

    /// Default pool bounds (documented; `db_pool_config` overrides).
    pub const DEFAULT_MAX_CONNS: u32 = 8;
    pub const DEFAULT_CHECKOUT_TIMEOUT_MS: u64 = 5000;

    /// Pool key: driver discriminant + full DSN remainder (the
    /// password is part of the key — different credentials never
    /// share handles).
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct PoolKey {
        driver: u8,
        dsn: String,
    }

    /// One pooled native handle. Pointer-sized, never dereferenced
    /// except by the single pool job holding it (same discipline as
    /// the TLS `Session`); closed on pool teardown or checkin to a
    /// retired pool.
    struct RawConn {
        driver: Driver,
        ptr: *mut std::ffi::c_void,
    }
    // SAFETY: single-owner use (checkout holds the pool lock across
    // handoff; the handle is used only by its pool job, then
    // checked back in or closed under the same lock).
    unsafe impl Send for RawConn {}

    struct PoolState {
        idle: Vec<RawConn>,
        total: u32,
        max: u32,
        checkout_timeout_ms: u64,
        /// Bumped by `db_close_pool`: checkins to a retired
        /// generation close instead of recycle (in-flight jobs are
        /// unaffected — they hold their handle).
        generation: u64,
        retired: bool,
    }

    struct Pool {
        state: Mutex<PoolState>,
        cv: Condvar,
    }

    static POOLS: LazyLock<Mutex<HashMap<PoolKey, std::sync::Arc<Pool>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn pool_key(driver: Driver, dsn: &str) -> PoolKey {
        let driver_n = match driver {
            Driver::Sqlite => 0,
            #[cfg(target_os = "linux")]
            Driver::Postgres => 1,
            #[cfg(target_os = "linux")]
            Driver::Mysql => 2,
        };
        PoolKey {
            driver: driver_n,
            dsn: dsn.to_string(),
        }
    }

    fn get_pool(driver: Driver, dsn: &str) -> std::sync::Arc<Pool> {
        let key = pool_key(driver, dsn);
        let mut pools = POOLS.lock().expect("db pool table lock");
        if let Some(p) = pools.get(&key) {
            return std::sync::Arc::clone(p);
        }
        // `:memory:` SQLite databases live inside one connection,
        // so their pool is pinned to a single handle (documented):
        // every checkout serializes onto the same connection.
        let max = if driver == Driver::Sqlite && dsn == "sqlite::memory:" {
            1
        } else {
            DEFAULT_MAX_CONNS
        };
        let pool = std::sync::Arc::new(Pool {
            state: Mutex::new(PoolState {
                idle: Vec::new(),
                total: 0,
                max,
                checkout_timeout_ms: DEFAULT_CHECKOUT_TIMEOUT_MS,
                generation: 0,
                retired: false,
            }),
            cv: Condvar::new(),
        });
        pools.insert(key, std::sync::Arc::clone(&pool));
        pool
    }

    /// Configure a pool (max connections, checkout timeout ms).
    /// Creates the entry when missing so config precedes first use.
    pub fn db_pool_config(dsn: &str, max: u32, timeout_ms: u64) -> Result<(), String> {
        let parsed = parse_dsn(dsn)?;
        if max == 0 {
            return Err("db pool max must be >= 1".to_string());
        }
        let pool = get_pool(parsed.driver, dsn);
        let mut st = pool.state.lock().expect("db pool lock");
        st.max = max;
        st.checkout_timeout_ms = timeout_ms;
        pool.cv.notify_all();
        Ok(())
    }

    /// Retire a pool: idle handles close now; in-flight handles
    /// close on checkin. Test isolation and clean shutdown.
    pub fn db_close_pool(dsn: &str) {
        let parsed = match parse_dsn(dsn) {
            Ok(p) => p,
            Err(_) => return,
        };
        let key = pool_key(parsed.driver, dsn);
        let pool = {
            let mut pools = POOLS.lock().expect("db pool table lock");
            match pools.remove(&key) {
                Some(p) => p,
                None => return,
            }
        };
        let mut st = pool.state.lock().expect("db pool lock");
        st.retired = true;
        st.generation += 1;
        for conn in st.idle.drain(..) {
            close_raw(conn);
        }
        pool.cv.notify_all();
    }

    /// RAII checkout guard: returns the handle to its pool on drop
    /// (or closes it when the pool retired meanwhile).
    pub struct Checkout {
        pool: Option<std::sync::Arc<Pool>>,
        conn: Option<RawConn>,
        generation: u64,
    }

    impl Checkout {
        pub fn ptr(&self) -> *mut std::ffi::c_void {
            self.conn.as_ref().expect("live checkout").ptr
        }
        pub fn driver(&self) -> Driver {
            self.conn.as_ref().expect("live checkout").driver
        }
    }

    impl Drop for Checkout {
        fn drop(&mut self) {
            if let Some(conn) = self.conn.take() {
                if let Some(pool) = self.pool.take() {
                    let mut st = pool.state.lock().expect("db pool lock");
                    if st.retired || st.generation != self.generation {
                        st.total = st.total.saturating_sub(1);
                        drop(st);
                        close_raw(conn);
                    } else {
                        st.idle.push(conn);
                    }
                    pool.cv.notify_one();
                } else {
                    close_raw(conn);
                }
            }
        }
    }

    /// Check out one connection, waiting up to the configured
    /// timeout. Pool exhaustion is a loud `Err` (documented
    /// message), never a silent stall. Connect failures are loud
    /// `Err`s and consume no pool slot.
    pub fn checkout(driver: Driver, dsn: &str) -> Result<Checkout, String> {
        let pool = get_pool(driver, dsn);
        let deadline = Instant::now()
            + Duration::from_millis(pool.state.lock().expect("db pool lock").checkout_timeout_ms);
        let mut st = pool.state.lock().expect("db pool lock");
        let generation = st.generation;
        loop {
            if st.retired || st.generation != generation {
                return Err("db pool retired during checkout".to_string());
            }
            if let Some(conn) = st.idle.pop() {
                let generation = st.generation;
                drop(st);
                return Ok(Checkout {
                    pool: Some(pool),
                    conn: Some(conn),
                    generation,
                });
            }
            if st.total < st.max {
                st.total += 1;
                let generation = st.generation;
                drop(st);
                match connect_raw(driver, dsn) {
                    Ok(ptr) => {
                        return Ok(Checkout {
                            pool: Some(pool),
                            conn: Some(RawConn { driver, ptr }),
                            generation,
                        });
                    }
                    Err(e) => {
                        let mut st = pool.state.lock().expect("db pool lock");
                        st.total = st.total.saturating_sub(1);
                        pool.cv.notify_one();
                        return Err(e);
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                let (max, total) = (st.max, st.total);
                return Err(format!(
                    "pool exhausted ({total} connections in use, max {max})"
                ));
            }
            let timeout = deadline - now;
            let (guard, _) = pool.cv.wait_timeout(st, timeout).expect("db pool condvar");
            st = guard;
        }
    }

    // ------------------------------------------------------------------
    // Text framing (boundary encoding, NOT a wire protocol).
    //
    // The FFI boundary only carries C scalars, `*c_char`, and
    // `Array<int>` (checked code cannot name structs in foreign
    // signatures — no checker change, no new types), so parameter
    // lists cross as length-prefixed text:
    //   params: `P<n>:` then per param `kind,len:payload;`
    // `kind` is the `DbCell` tag (0 null, 1 int, 2 float, 3 text,
    // 4 bool); `len` is the payload CHARACTER count (Toto `len()`
    // counts Unicode scalar values, and the parser consumes
    // characters — multibyte text round-trips exactly); the
    // payload is the int decimal, the Rust-debug float (Toto
    // `to_string` renders floats the same way — exact round-trip),
    // the raw text, `0`/`1` for bools, or empty for null. Counts
    // are read first and payloads consumed by length, so quoted
    // separators inside payloads can never confuse the parse.
    // NUL bytes cannot occur (rejected at the FFI boundary).
    // Pure Toto builds params with the same grammar
    // (`gzv.std/db`); rows come back as `DbVal` structs on both
    // engines (the runtime executor converts `RtVal`s directly and
    // never touches this framing).
    // ------------------------------------------------------------------

    /// Render cells as a params framing string (lengths are
    /// character counts — see the module contract).
    pub fn encode_params(cells: &[DbCell]) -> String {
        let mut out = String::with_capacity(cells.len() * 8);
        out.push('P');
        out.push_str(&cells.len().to_string());
        out.push(':');
        for c in cells {
            let payload = match c.kind {
                CELL_INT | CELL_BOOL => c.i.to_string(),
                CELL_FLOAT => format!("{:?}", c.f),
                CELL_TEXT => c.s.clone(),
                _ => String::new(),
            };
            out.push_str(&c.kind.to_string());
            out.push(',');
            out.push_str(&payload.chars().count().to_string());
            out.push(':');
            out.push_str(&payload);
            out.push(';');
        }
        out
    }

    struct Framing<'a> {
        b: &'a [u8],
        i: usize,
    }

    impl<'a> Framing<'a> {
        fn new(b: &'a [u8]) -> Self {
            Self { b, i: 0 }
        }
        fn expect(&mut self, want: u8, what: &str) -> Result<(), String> {
            match self.b.get(self.i) {
                Some(&c) if c == want => {
                    self.i += 1;
                    Ok(())
                }
                _ => Err(format!("bad db framing (want {what})")),
            }
        }
        fn number(&mut self) -> Result<usize, String> {
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i == start {
                return Err("bad db framing (want count)".to_string());
            }
            std::str::from_utf8(&self.b[start..self.i])
                .map_err(|_| "bad db framing (count)".to_string())?
                .parse::<usize>()
                .map_err(|_| "bad db framing (count)".to_string())
        }
        fn cell(&mut self) -> Result<DbCell, String> {
            let kind = self.number()?;
            self.expect(b',', "','")?;
            let len = self.number()?;
            self.expect(b':', "':'")?;
            // Consume `len` CHARACTERS (Toto `len()` counts Unicode
            // scalar values): walk UTF-8 sequence starts so
            // multibyte text split across the payload never
            // corrupts; invalid bytes fail loudly, never silently.
            let rest = self
                .b
                .get(self.i..)
                .ok_or_else(|| "bad db framing (short payload)".to_string())?;
            let text = std::str::from_utf8(rest)
                .map_err(|_| "bad db framing (payload not UTF-8)".to_string())?;
            let mut chars = text.chars();
            let mut end = 0usize;
            for _ in 0..len {
                match chars.next() {
                    Some(c) => {
                        end += c.len_utf8();
                    }
                    None => {
                        return Err("bad db framing (short payload)".to_string());
                    }
                }
            }
            let text = text[..end].to_string();
            self.i += end;
            self.expect(b';', "';'")?;
            Ok(match kind {
                0 => DbCell::null(),
                1 => text
                    .parse::<i64>()
                    .map(DbCell::int)
                    .unwrap_or_else(|_| DbCell::text(text)),
                2 => text
                    .parse::<f64>()
                    .map(DbCell::float)
                    .unwrap_or_else(|_| DbCell::text(text)),
                3 => DbCell::text(text),
                4 => match text.as_str() {
                    "1" => DbCell::bool_(true),
                    "0" => DbCell::bool_(false),
                    _ => DbCell::text(text),
                },
                _ => return Err(format!("bad db framing (kind {kind})")),
            })
        }
        fn end(&self) -> Result<(), String> {
            if self.i == self.b.len() {
                Ok(())
            } else {
                Err("bad db framing (trailing bytes)".to_string())
            }
        }
    }

    /// Render rows as a rows framing byte string (the interpreter
    /// path returns this as a Toto string; pure Toto decodes it
    /// with the same grammar — see `gzv.std/db`). Lengths are
    /// character counts, same contract as params.
    pub fn encode_rows(rows: &[Vec<DbCell>]) -> Vec<u8> {
        let mut out = String::with_capacity(rows.len() * 16);
        out.push('R');
        out.push_str(&rows.len().to_string());
        out.push(':');
        for row in rows {
            out.push('C');
            out.push_str(&row.len().to_string());
            out.push(':');
            for c in row {
                let payload = match c.kind {
                    CELL_INT | CELL_BOOL => c.i.to_string(),
                    CELL_FLOAT => format!("{:?}", c.f),
                    CELL_TEXT => c.s.clone(),
                    _ => String::new(),
                };
                out.push_str(&c.kind.to_string());
                out.push(',');
                out.push_str(&payload.chars().count().to_string());
                out.push(':');
                out.push_str(&payload);
                out.push(';');
            }
        }
        out.into_bytes()
    }

    /// Parse a params framing string (Toto-built) into cells.
    pub fn decode_params(s: &str) -> Result<Vec<DbCell>, String> {
        let mut f = Framing::new(s.as_bytes());
        f.expect(b'P', "'P'")?;
        let n = f.number()?;
        f.expect(b':', "':'")?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(f.cell()?);
        }
        f.end()?;
        Ok(out)
    }

    /// One statement: checkout, run, checkin (via `Checkout`
    /// drop). Returns affected rows for DML (exact on all
    /// drivers); DDL counts are driver-defined (SQLite reports
    /// its internal counter, pg/mysql report 0 — assert success,
    /// not the count). SELECT paths use `db_query` instead —
    /// running a query through `exec` discards rows.
    pub fn db_exec(dsn: &str, sql: &str, params: &[DbCell]) -> Result<i64, String> {
        let parsed = parse_dsn(dsn)?;
        let co = checkout(parsed.driver, dsn)?;
        exec_raw(&co, sql, params)
    }

    /// One query: checkout, run, collect all rows, checkin.
    pub fn db_query(dsn: &str, sql: &str, params: &[DbCell]) -> Result<Vec<Vec<DbCell>>, String> {
        let parsed = parse_dsn(dsn)?;
        let co = checkout(parsed.driver, dsn)?;
        query_raw(&co, sql, params)
    }

    // Driver vtable (implemented per driver below; the pool core
    // above never names a driver symbol directly).
    fn connect_raw(driver: Driver, dsn: &str) -> Result<*mut std::ffi::c_void, String> {
        match driver {
            Driver::Sqlite => sqlite::connect(dsn),
            #[cfg(target_os = "linux")]
            Driver::Postgres => pg::connect(dsn),
            #[cfg(target_os = "linux")]
            Driver::Mysql => my::connect(dsn),
        }
    }

    fn close_raw(conn: RawConn) {
        match conn.driver {
            Driver::Sqlite => sqlite::close(conn.ptr),
            #[cfg(target_os = "linux")]
            Driver::Postgres => pg::close(conn.ptr),
            #[cfg(target_os = "linux")]
            Driver::Mysql => my::close(conn.ptr),
        }
    }

    fn exec_raw(co: &Checkout, sql: &str, params: &[DbCell]) -> Result<i64, String> {
        match co.driver() {
            Driver::Sqlite => sqlite::exec(co.ptr(), sql, params),
            #[cfg(target_os = "linux")]
            Driver::Postgres => pg::exec(co.ptr(), sql, params),
            #[cfg(target_os = "linux")]
            Driver::Mysql => my::exec(co.ptr(), sql, params),
        }
    }

    fn query_raw(co: &Checkout, sql: &str, params: &[DbCell]) -> Result<Vec<Vec<DbCell>>, String> {
        match co.driver() {
            Driver::Sqlite => sqlite::query(co.ptr(), sql, params),
            #[cfg(target_os = "linux")]
            Driver::Postgres => pg::query(co.ptr(), sql, params),
            #[cfg(target_os = "linux")]
            Driver::Mysql => my::query(co.ptr(), sql, params),
        }
    }

    // ------------------------------------------------------------------
    // SQLite (unix-wide, system libsqlite3).
    // ------------------------------------------------------------------
    mod sqlite {
        use super::{CELL_BOOL, CELL_FLOAT, CELL_INT, CELL_TEXT, DbCell};
        use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_void};

        enum Db {}
        enum Stmt {}

        const OK: c_int = 0;
        const ROW: c_int = 100;
        const DONE: c_int = 101;
        const OPEN_READWRITE: c_int = 0x2;
        const OPEN_CREATE: c_int = 0x4;
        const INTEGER: c_int = 1;
        const FLOAT: c_int = 2;
        const TEXT: c_int = 3;
        // BLOB = 4 (arrives as text of raw bytes, lossy for
        // non-UTF8 — v1 has no blob cell; documented).
        const NULL: c_int = 5;

        #[link(name = "sqlite3")]
        unsafe extern "C" {
            fn sqlite3_open_v2(
                filename: *const c_char,
                pp_db: *mut *mut Db,
                flags: c_int,
                z_vfs: *const c_char,
            ) -> c_int;
            fn sqlite3_busy_timeout(db: *mut Db, ms: c_int) -> c_int;
            fn sqlite3_close_v2(db: *mut Db) -> c_int;
            fn sqlite3_prepare_v2(
                db: *mut Db,
                z_sql: *const c_char,
                n_byte: c_int,
                pp_stmt: *mut *mut Stmt,
                pz_tail: *mut *const c_char,
            ) -> c_int;
            fn sqlite3_bind_int64(stmt: *mut Stmt, idx: c_int, v: i64) -> c_int;
            fn sqlite3_bind_double(stmt: *mut Stmt, idx: c_int, v: f64) -> c_int;
            fn sqlite3_bind_text(
                stmt: *mut Stmt,
                idx: c_int,
                v: *const c_char,
                n: c_int,
                d: *const c_void,
            ) -> c_int;
            fn sqlite3_bind_null(stmt: *mut Stmt, idx: c_int) -> c_int;
            fn sqlite3_step(stmt: *mut Stmt) -> c_int;
            fn sqlite3_column_count(stmt: *mut Stmt) -> c_int;
            fn sqlite3_column_type(stmt: *mut Stmt, i: c_int) -> c_int;
            fn sqlite3_column_int64(stmt: *mut Stmt, i: c_int) -> i64;
            fn sqlite3_column_double(stmt: *mut Stmt, i: c_int) -> f64;
            fn sqlite3_column_text(stmt: *mut Stmt, i: c_int) -> *const c_uchar;
            fn sqlite3_column_bytes(stmt: *mut Stmt, i: c_int) -> c_int;
            fn sqlite3_finalize(stmt: *mut Stmt) -> c_int;
            fn sqlite3_errmsg(db: *mut Db) -> *const c_char;
            fn sqlite3_changes(db: *mut Db) -> c_int;
        }

        fn err(db: *mut Db, what: &str) -> String {
            // SAFETY: `errmsg` returns a live NUL string for a live db.
            let msg = unsafe {
                let p = sqlite3_errmsg(db);
                if p.is_null() {
                    "unknown error".to_string()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            format!("sqlite {what}: {msg}")
        }

        fn is_memory(dsn: &str) -> bool {
            dsn == "sqlite::memory:"
        }

        fn path_of(dsn: &str) -> Result<String, String> {
            if is_memory(dsn) {
                return Ok(":memory:".to_string());
            }
            dsn.strip_prefix("sqlite:path=")
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    "bad sqlite DSN (want sqlite:path=/x/y.db or sqlite::memory:)".to_string()
                })
        }

        pub fn connect(dsn: &str) -> Result<*mut c_void, String> {
            let path = path_of(dsn)?;
            let cpath =
                CString::new(path.clone()).map_err(|_| "bad sqlite path (NUL byte)".to_string())?;
            let mut db: *mut Db = std::ptr::null_mut();
            // SAFETY: `cpath` outlives the call; `db` is written on
            // success only (null-checked below).
            let r = unsafe {
                sqlite3_open_v2(
                    cpath.as_ptr(),
                    &mut db as *mut *mut Db,
                    OPEN_READWRITE | OPEN_CREATE,
                    std::ptr::null(),
                )
            };
            if r != OK || db.is_null() {
                return Err(format!("sqlite open {path:?} failed (code {r})"));
            }
            // Pooled handles wait inside SQLite on lock contention
            // instead of failing SQLITE_BUSY (pool-under-load rule).
            unsafe {
                sqlite3_busy_timeout(db, 5000);
            }
            Ok(db as *mut c_void)
        }

        pub fn close(ptr: *mut c_void) {
            // SAFETY: live pooled handle, single owner (pool Drop).
            unsafe {
                sqlite3_close_v2(ptr as *mut Db);
            }
        }

        /// `SQLITE_TRANSIENT` (-1): SQLite copies the bound text.
        const TRANSIENT: *const c_void = -1isize as *const c_void;

        fn bind_all(db: *mut Db, stmt: *mut Stmt, params: &[DbCell]) -> Result<(), String> {
            for (i, p) in params.iter().enumerate() {
                let idx = (i + 1) as c_int;
                // SAFETY: `stmt` is live; values outlive the step
                // (`TRANSIENT` copies text in).
                let r = unsafe {
                    match p.kind {
                        CELL_INT | CELL_BOOL => sqlite3_bind_int64(stmt, idx, p.i),
                        CELL_FLOAT => sqlite3_bind_double(stmt, idx, p.f),
                        CELL_TEXT => {
                            let bytes = p.s.as_bytes();
                            sqlite3_bind_text(
                                stmt,
                                idx,
                                bytes.as_ptr() as *const c_char,
                                bytes.len() as c_int,
                                TRANSIENT,
                            )
                        }
                        _ => sqlite3_bind_null(stmt, idx),
                    }
                };
                if r != OK {
                    return Err(err(db, "bind"));
                }
            }
            Ok(())
        }

        fn prepare(db: *mut Db, sql: &str) -> Result<*mut Stmt, String> {
            let csql = CString::new(sql).map_err(|_| "bad SQL (NUL byte)".to_string())?;
            let mut stmt: *mut Stmt = std::ptr::null_mut();
            // SAFETY: `csql` outlives the call; `stmt` null-checked.
            let r = unsafe {
                sqlite3_prepare_v2(
                    db,
                    csql.as_ptr(),
                    -1,
                    &mut stmt as *mut *mut Stmt,
                    std::ptr::null_mut(),
                )
            };
            if r != OK || stmt.is_null() {
                return Err(err(db, "prepare"));
            }
            Ok(stmt)
        }

        fn read_row(stmt: *mut Stmt) -> Vec<DbCell> {
            // SAFETY: `stmt` just stepped ROW; column pointers are
            // valid until the next step (copied out below).
            unsafe {
                let n = sqlite3_column_count(stmt);
                let mut row = Vec::with_capacity(n as usize);
                for i in 0..n {
                    let t = sqlite3_column_type(stmt, i);
                    row.push(match t {
                        INTEGER => DbCell::int(sqlite3_column_int64(stmt, i)),
                        FLOAT => DbCell::float(sqlite3_column_double(stmt, i)),
                        TEXT => {
                            let p = sqlite3_column_text(stmt, i);
                            let len = sqlite3_column_bytes(stmt, i) as usize;
                            if p.is_null() {
                                DbCell::null()
                            } else {
                                let bytes = std::slice::from_raw_parts(p, len);
                                DbCell::text(String::from_utf8_lossy(bytes).into_owned())
                            }
                        }
                        NULL => DbCell::null(),
                        // BLOB (4): raw bytes as lossy text (no blob
                        // cell in v1 — documented).
                        _ => {
                            let p = sqlite3_column_text(stmt, i);
                            let len = sqlite3_column_bytes(stmt, i) as usize;
                            if p.is_null() {
                                DbCell::null()
                            } else {
                                let bytes = std::slice::from_raw_parts(p, len);
                                DbCell {
                                    kind: CELL_TEXT,
                                    i: 0,
                                    f: 0.0,
                                    s: String::from_utf8_lossy(bytes).into_owned(),
                                }
                            }
                        }
                    });
                }
                row
            }
        }

        /// Empty SQL (no statement) is a no-op success (`Ok(0)`),
        /// matching the `db_exec` contract for DDL batches.
        pub fn exec(ptr: *mut c_void, sql: &str, params: &[DbCell]) -> Result<i64, String> {
            let db = ptr as *mut Db;
            if sql.trim().is_empty() {
                return Ok(0);
            }
            let stmt = prepare(db, sql)?;
            let r = bind_all(db, stmt, params).and_then(|()| {
                // SAFETY: prepared + bound; step to completion.
                let r = unsafe { sqlite3_step(stmt) };
                if r == DONE || r == ROW {
                    // SAFETY: `db` is live.
                    Ok(unsafe { sqlite3_changes(db) } as i64)
                } else {
                    Err(err(db, "step"))
                }
            });
            // SAFETY: prepared here; single owner.
            unsafe {
                sqlite3_finalize(stmt);
            }
            r
        }

        pub fn query(
            ptr: *mut c_void,
            sql: &str,
            params: &[DbCell],
        ) -> Result<Vec<Vec<DbCell>>, String> {
            let db = ptr as *mut Db;
            let stmt = prepare(db, sql)?;
            let mut rows = Vec::new();
            let r = (|| {
                bind_all(db, stmt, params)?;
                loop {
                    // SAFETY: prepared + bound; rows copied out.
                    let r = unsafe { sqlite3_step(stmt) };
                    match r {
                        ROW => rows.push(read_row(stmt)),
                        DONE => return Ok(()),
                        _ => return Err(err(db, "step")),
                    }
                }
            })();
            // SAFETY: prepared here; single owner.
            unsafe {
                sqlite3_finalize(stmt);
            }
            r.map(|()| rows)
        }
    }

    // ------------------------------------------------------------------
    // Postgres (Linux-only, system libpq).
    // ------------------------------------------------------------------
    #[cfg(target_os = "linux")]
    mod pg {
        use super::{DbCell, translate_pg};
        use std::ffi::{CStr, CString, c_char, c_int, c_void};

        enum Conn {}
        enum Res {}

        const CONNECTION_OK: c_int = 0;
        const PGRES_COMMAND_OK: c_int = 1;
        const PGRES_TUPLES_OK: c_int = 2;
        // Type OIDs (stable catalog numbers): decode ints, floats,
        // bools natively; numerics + everything else arrive as text
        // (documented — numeric precision is never silently lost).
        const OID_BOOL: u32 = 16;
        const OID_INT2: u32 = 21;
        const OID_INT4: u32 = 23;
        const OID_INT8: u32 = 20;
        const OID_FLOAT4: u32 = 700;
        const OID_FLOAT8: u32 = 701;

        #[link(name = "pq")]
        unsafe extern "C" {
            fn PQconnectdb(conninfo: *const c_char) -> *mut Conn;
            fn PQstatus(conn: *const Conn) -> c_int;
            fn PQfinish(conn: *mut Conn);
            fn PQsetNoticeProcessor(
                conn: *mut Conn,
                proc: Option<unsafe extern "C" fn(arg: *mut c_void, message: *const c_char)>,
                arg: *mut c_void,
            ) -> *mut c_void;
            fn PQexecParams(
                conn: *mut Conn,
                command: *const c_char,
                n_params: c_int,
                param_types: *const u32,
                param_values: *const *const c_char,
                param_lengths: *const c_int,
                param_formats: *const c_int,
                result_format: c_int,
            ) -> *mut Res;
            fn PQresultStatus(res: *const Res) -> c_int;
            fn PQntuples(res: *const Res) -> c_int;
            fn PQnfields(res: *const Res) -> c_int;
            fn PQftype(res: *const Res, col: c_int) -> u32;
            fn PQgetvalue(res: *const Res, row: c_int, col: c_int) -> *const c_char;
            fn PQgetlength(res: *const Res, row: c_int, col: c_int) -> c_int;
            fn PQgetisnull(res: *const Res, row: c_int, col: c_int) -> c_int;
            fn PQcmdTuples(res: *mut Res) -> *const c_char;
            fn PQclear(res: *mut Res);
            fn PQerrorMessage(conn: *const Conn) -> *const c_char;
        }

        /// Notice sink: drop server notices (see `connect`). No
        /// logging here — pool threads must stay silent and never
        /// touch shared state.
        unsafe extern "C" fn swallow_notice(_arg: *mut c_void, _message: *const c_char) {}

        fn conn_err(conn: *mut Conn, what: &str) -> String {
            // SAFETY: `errorMessage` returns a live NUL string.
            let msg = unsafe {
                let p = PQerrorMessage(conn as *const Conn);
                if p.is_null() {
                    "unknown error".to_string()
                } else {
                    CStr::from_ptr(p).to_string_lossy().trim().to_owned()
                }
            };
            format!("postgres {what}: {msg}")
        }

        /// `k=v` DSN remainder → libpq conninfo (plus a default
        /// `connect_timeout=10` when unset — connect-timeout policy).
        fn conninfo(rest: &str) -> Result<CString, String> {
            let mut info = rest.to_string();
            if !rest.contains("connect_timeout") {
                info.push_str(" connect_timeout=10");
            }
            CString::new(info).map_err(|_| "bad postgres DSN (NUL byte)".to_string())
        }

        pub fn connect(dsn: &str) -> Result<*mut c_void, String> {
            let rest = dsn.strip_prefix("postgres:").ok_or_else(|| {
                "bad postgres DSN (want postgres:host=.. dbname=.. ...)".to_string()
            })?;
            let info = conninfo(rest)?;
            // SAFETY: `info` outlives the call; null-checked.
            let conn = unsafe { PQconnectdb(info.as_ptr()) };
            if conn.is_null() {
                return Err("postgres connect: allocation failed".to_string());
            }
            // SAFETY: `conn` is live.
            if unsafe { PQstatus(conn as *const Conn) } != CONNECTION_OK {
                let e = conn_err(conn, "connect");
                unsafe { PQfinish(conn) };
                return Err(e);
            }
            // Silence server notices (NOTICEs would go to stderr and
            // pollute golden output; errors still surface through
            // `PQerrorMessage`). A no-op processor replaces the
            // default stderr printer.
            unsafe {
                PQsetNoticeProcessor(conn, Some(swallow_notice), std::ptr::null_mut());
            }
            Ok(conn as *mut c_void)
        }

        pub fn close(ptr: *mut c_void) {
            // SAFETY: live pooled handle, single owner.
            unsafe { PQfinish(ptr as *mut Conn) };
        }

        /// Bind params as text (server-side binding — `PQexecParams`
        /// with null formats/types; the server parses per the query).
        /// Returns owned CStrings plus pointer/length vectors.
        #[allow(clippy::type_complexity)]
        fn bind_params(
            params: &[DbCell],
        ) -> Result<(Vec<CString>, Vec<*const c_char>, Vec<c_int>), String> {
            let mut owned = Vec::with_capacity(params.len());
            for p in params {
                let s = match p.kind {
                    super::CELL_INT | super::CELL_BOOL => p.i.to_string(),
                    super::CELL_FLOAT => {
                        if p.f.is_nan() {
                            "NaN".to_string()
                        } else if p.f.is_infinite() {
                            if p.f > 0.0 {
                                "Infinity".to_string()
                            } else {
                                "-Infinity".to_string()
                            }
                        } else {
                            format!("{:?}", p.f)
                        }
                    }
                    super::CELL_TEXT => p.s.clone(),
                    _ => String::new(),
                };
                owned.push(CString::new(s).map_err(|_| "bad param (NUL byte)".to_string())?);
            }
            let ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
            // Null params: pass a null pointer slot (libpq treats a
            // null value pointer as SQL NULL).
            let mut ptrs2 = ptrs;
            let mut lens = vec![0 as c_int; params.len()];
            for (i, p) in params.iter().enumerate() {
                if p.kind == super::CELL_NULL {
                    ptrs2[i] = std::ptr::null();
                } else {
                    lens[i] = owned[i].as_bytes().len() as c_int;
                }
            }
            Ok((owned, ptrs2, lens))
        }

        fn decode(oid: u32, text: &str) -> DbCell {
            match oid {
                OID_INT2 | OID_INT4 | OID_INT8 => text
                    .parse::<i64>()
                    .map(DbCell::int)
                    .unwrap_or_else(|_| DbCell::text(text.to_string())),
                OID_FLOAT4 | OID_FLOAT8 => text
                    .parse::<f64>()
                    .map(DbCell::float)
                    .unwrap_or_else(|_| DbCell::text(text.to_string())),
                OID_BOOL => match text {
                    "t" => DbCell::bool_(true),
                    "f" => DbCell::bool_(false),
                    _ => DbCell::text(text.to_string()),
                },
                // Numerics + everything else: text (precision kept).
                _ => DbCell::text(text.to_string()),
            }
        }

        fn run(conn: *mut Conn, sql: &str, params: &[DbCell]) -> Result<*mut Res, String> {
            let translated = translate_pg(sql);
            let csql = CString::new(translated).map_err(|_| "bad SQL (NUL byte)".to_string())?;
            let (_owned, ptrs, lens) = bind_params(params)?;
            let pvalues = if params.is_empty() {
                std::ptr::null()
            } else {
                ptrs.as_ptr()
            };
            let plens = if params.is_empty() {
                std::ptr::null()
            } else {
                lens.as_ptr()
            };
            // SAFETY: `csql`/param vectors outlive the call; text
            // format both ways (null formats/types/result).
            let res = unsafe {
                PQexecParams(
                    conn,
                    csql.as_ptr(),
                    params.len() as c_int,
                    std::ptr::null(),
                    pvalues,
                    plens,
                    std::ptr::null(),
                    0,
                )
            };
            if res.is_null() {
                return Err(conn_err(conn, "exec"));
            }
            // SAFETY: `res` is live.
            match unsafe { PQresultStatus(res as *const Res) } {
                PGRES_COMMAND_OK | PGRES_TUPLES_OK => Ok(res),
                _ => {
                    let e = conn_err(conn, "exec");
                    unsafe { PQclear(res) };
                    Err(e)
                }
            }
        }

        pub fn exec(ptr: *mut c_void, sql: &str, params: &[DbCell]) -> Result<i64, String> {
            let conn = ptr as *mut Conn;
            if sql.trim().is_empty() {
                return Ok(0);
            }
            let res = run(conn, sql, params)?;
            // SAFETY: command-OK result; `cmdTuples` is "" for DDL
            // and row-returning statements (callers use `query`
            // there) — those report 0 affected rows, not -1, so a
            // successful `exec` never needs the error slot.
            let n = unsafe {
                let p = PQcmdTuples(res);
                let s = if p.is_null() {
                    ""
                } else {
                    &CStr::from_ptr(p).to_string_lossy()
                };
                let n = s.trim().parse::<i64>().unwrap_or(0);
                PQclear(res);
                n
            };
            Ok(n)
        }

        pub fn query(
            ptr: *mut c_void,
            sql: &str,
            params: &[DbCell],
        ) -> Result<Vec<Vec<DbCell>>, String> {
            let conn = ptr as *mut Conn;
            let res = run(conn, sql, params)?;
            // SAFETY: tuples-OK result; values copied out as text.
            let rows = unsafe {
                let nrows = PQntuples(res as *const Res);
                let ncols = PQnfields(res as *const Res);
                let mut rows = Vec::with_capacity(nrows as usize);
                for r in 0..nrows {
                    let mut row = Vec::with_capacity(ncols as usize);
                    for c in 0..ncols {
                        if PQgetisnull(res as *const Res, r, c) != 0 {
                            row.push(DbCell::null());
                            continue;
                        }
                        let p = PQgetvalue(res as *const Res, r, c);
                        let len = PQgetlength(res as *const Res, r, c) as usize;
                        let bytes = std::slice::from_raw_parts(p as *const u8, len);
                        let text = String::from_utf8_lossy(bytes);
                        let oid = PQftype(res as *const Res, c);
                        row.push(decode(oid, &text));
                    }
                    rows.push(row);
                }
                PQclear(res);
                rows
            };
            Ok(rows)
        }
    }

    // ------------------------------------------------------------------
    // MySQL (Linux-only, system libmysqlclient).
    //
    // Value decode rule (documented wart): the text protocol carries
    // no column types on this path (reading `MYSQL_FIELD.type`
    // would hand-roll a version-drifting struct layout — rejected
    // per the Wave 1 addrinfo lesson), so values decode by shape:
    // integer-shaped → int, float-shaped → float, else text; NULL
    // is exact (null row slots, not text). A TEXT column holding
    // "123" therefore reads as int — compare loosely or CAST in
    // SQL when the distinction matters. Parameters bind through
    // the library's own `mysql_real_escape_string` (charset-aware,
    // provided by the mature library — NOT Toto escaping).
    // ------------------------------------------------------------------
    #[cfg(target_os = "linux")]
    mod my {
        use super::{DbCell, split_placeholders};
        use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong, c_void};

        enum Mysql {}
        enum Res {}

        const OPT_CONNECT_TIMEOUT: c_int = 0;
        const OPT_READ_TIMEOUT: c_int = 11;
        const OPT_WRITE_TIMEOUT: c_int = 12;

        /// Process-wide client-library init. `mysql_server_init`
        /// (the real symbol behind the `mysql_library_init` macro)
        /// is NOT thread-safe against concurrent first use, and
        /// pool threads connect concurrently — so the first
        /// connection serializes init once (`LazyLock` is
        /// thread-safe by construction).
        static LIB_INIT: std::sync::LazyLock<()> = std::sync::LazyLock::new(|| {
            // SAFETY: runs exactly once, before any connection.
            unsafe {
                mysql_server_init(0, std::ptr::null(), std::ptr::null());
            }
        });

        #[link(name = "mysqlclient")]
        unsafe extern "C" {
            fn mysql_server_init(
                argc: c_int,
                argv: *const *const c_char,
                groups: *const *const c_char,
            ) -> c_int;
            fn mysql_init(mysql: *mut Mysql) -> *mut Mysql;
            fn mysql_options(mysql: *mut Mysql, option: c_int, arg: *const c_void) -> c_int;
            fn mysql_real_connect(
                mysql: *mut Mysql,
                host: *const c_char,
                user: *const c_char,
                passwd: *const c_char,
                db: *const c_char,
                port: c_uint,
                unix_socket: *const c_char,
                clientflag: c_ulong,
            ) -> *mut Mysql;
            fn mysql_set_character_set(mysql: *mut Mysql, csname: *const c_char) -> c_int;
            fn mysql_real_query(mysql: *mut Mysql, q: *const c_char, length: c_ulong) -> c_int;
            fn mysql_store_result(mysql: *mut Mysql) -> *mut Res;
            fn mysql_num_rows(res: *mut Res) -> u64;
            fn mysql_num_fields(res: *mut Res) -> c_uint;
            fn mysql_fetch_row(res: *mut Res) -> *mut *mut c_char;
            fn mysql_fetch_lengths(res: *mut Res) -> *mut c_ulong;
            fn mysql_free_result(res: *mut Res);
            fn mysql_affected_rows(mysql: *mut Mysql) -> u64;
            fn mysql_field_count(mysql: *mut Mysql) -> c_uint;
            fn mysql_error(mysql: *mut Mysql) -> *const c_char;
            fn mysql_errno(mysql: *mut Mysql) -> c_uint;
            fn mysql_close(mysql: *mut Mysql);
            fn mysql_real_escape_string(
                mysql: *mut Mysql,
                to: *mut c_char,
                from: *const c_char,
                length: c_ulong,
            ) -> c_ulong;
        }

        fn conn_err(conn: *mut Mysql, what: &str) -> String {
            // SAFETY: `mysql_error` returns a live NUL string.
            let (msg, code) = unsafe {
                let p = mysql_error(conn);
                let e = mysql_errno(conn);
                let s = if p.is_null() {
                    "unknown error".to_string()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                };
                (s, e)
            };
            format!("mysql {what}: {msg} (errno {code})")
        }

        /// Hold owned CStrings for one connection setup (host, user,
        /// password, db, charset): every pointer below borrows from
        /// this bundle, so the bundle outlives the connect call.
        struct ConnArgs {
            _host: CString,
            _user: CString,
            _passwd: CString,
            _db: CString,
            _charset: CString,
            port: c_uint,
            timeout: c_uint,
        }

        fn conn_args(rest: &str) -> Result<ConnArgs, String> {
            let kv = super::kv_pairs(rest);
            let req = |k: &str| {
                kv.get(k)
                    .cloned()
                    .ok_or_else(|| format!("bad mysql DSN (missing {k}=...)"))
            };
            let cstr = |s: &str, what: &str| {
                CString::new(s).map_err(|_| format!("bad mysql DSN ({what} has NUL byte)"))
            };
            let host = req("host")?;
            let user = req("user")?;
            let passwd = kv.get("password").cloned().unwrap_or_default();
            let db = kv.get("dbname").cloned().unwrap_or_default();
            let port = kv
                .get("port")
                .map(|p| p.parse::<c_uint>().unwrap_or(3306))
                .unwrap_or(3306);
            let timeout = kv
                .get("connect_timeout")
                .map(|p| p.parse::<c_uint>().unwrap_or(10))
                .unwrap_or(10);
            Ok(ConnArgs {
                _host: cstr(&host, "host")?,
                _user: cstr(&user, "user")?,
                _passwd: cstr(&passwd, "password")?,
                _db: cstr(&db, "dbname")?,
                _charset: cstr("utf8mb4", "charset")?,
                port,
                timeout,
            })
        }

        pub fn connect(dsn: &str) -> Result<*mut c_void, String> {
            let rest = dsn
                .strip_prefix("mysql:")
                .ok_or_else(|| "bad mysql DSN (want mysql:host=.. dbname=.. ...)".to_string())?;
            let a = conn_args(rest)?;
            std::sync::LazyLock::force(&LIB_INIT);
            // SAFETY: fresh handle, null-checked; `a` outlives setup.
            let conn = unsafe { mysql_init(std::ptr::null_mut()) };
            if conn.is_null() {
                return Err("mysql init: allocation failed".to_string());
            }
            let r = unsafe {
                let mut ok = mysql_options(
                    conn,
                    OPT_CONNECT_TIMEOUT,
                    &a.timeout as *const c_uint as *const c_void,
                );
                if ok == 0 {
                    ok = mysql_options(
                        conn,
                        OPT_READ_TIMEOUT,
                        &a.timeout as *const c_uint as *const c_void,
                    );
                }
                if ok == 0 {
                    ok = mysql_options(
                        conn,
                        OPT_WRITE_TIMEOUT,
                        &a.timeout as *const c_uint as *const c_void,
                    );
                }
                ok
            };
            if r != 0 {
                let e = conn_err(conn, "options");
                unsafe { mysql_close(conn) };
                return Err(e);
            }
            // SAFETY: `a` bundles every borrowed string.
            let ok = unsafe {
                mysql_real_connect(
                    conn,
                    a._host.as_ptr(),
                    a._user.as_ptr(),
                    a._passwd.as_ptr(),
                    a._db.as_ptr(),
                    a.port,
                    std::ptr::null(),
                    0,
                )
            };
            if ok.is_null() {
                let e = conn_err(conn, "connect");
                unsafe { mysql_close(conn) };
                return Err(e);
            }
            // SAFETY: connected handle.
            if unsafe { mysql_set_character_set(conn, a._charset.as_ptr()) } != 0 {
                let e = conn_err(conn, "charset");
                unsafe { mysql_close(conn) };
                return Err(e);
            }
            Ok(conn as *mut c_void)
        }

        pub fn close(ptr: *mut c_void) {
            // SAFETY: live pooled handle, single owner.
            unsafe { mysql_close(ptr as *mut Mysql) };
        }

        /// Escape one text param with the library's own function on
        /// the live connection (charset-aware). Worst case doubles
        /// the length plus quotes (allocate 2n+3).
        fn escape(conn: *mut Mysql, s: &str) -> String {
            let bytes = s.as_bytes();
            let mut buf = vec![0 as c_char; bytes.len() * 2 + 3];
            // SAFETY: `buf` sized for the worst case; `s` outlives.
            let n = unsafe {
                mysql_real_escape_string(
                    conn,
                    buf.as_mut_ptr(),
                    bytes.as_ptr() as *const c_char,
                    bytes.len() as c_ulong,
                )
            };
            buf.truncate(n as usize);
            // SAFETY: escaped output is plain bytes; quotes added by
            // the caller, not the library.
            let mut out = String::with_capacity(n as usize + 2);
            out.push('\'');
            out.push_str(&String::from_utf8_lossy(
                // SAFETY: `buf` holds `n` initialized entries.
                unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n as usize) },
            ));
            out.push('\'');
            out
        }

        /// Interleave literal segments with escaped params (the
        /// `?` walk skips literals/comments — shared scanner).
        /// Int/bool bind as bare literals, floats as literals,
        /// text via the library escaper, null as NULL.
        fn build_sql(conn: *mut Mysql, sql: &str, params: &[DbCell]) -> Result<String, String> {
            let segs = split_placeholders(sql);
            if segs.len() != params.len() + 1 {
                return Err(format!(
                    "mysql param count mismatch: {} placeholders, {} args",
                    segs.len() - 1,
                    params.len()
                ));
            }
            let mut out = String::with_capacity(sql.len() + 16);
            out.push_str(&segs[0]);
            for (seg, p) in segs[1..].iter().zip(params.iter()) {
                match p.kind {
                    super::CELL_INT | super::CELL_BOOL => out.push_str(&p.i.to_string()),
                    super::CELL_FLOAT => {
                        if p.f.is_nan() {
                            out.push_str("NULL");
                        } else if p.f.is_infinite() {
                            out.push_str(if p.f > 0.0 { "1e308" } else { "-1e308" });
                        } else {
                            out.push_str(&format!("{:?}", p.f));
                        }
                    }
                    super::CELL_TEXT => out.push_str(&escape(conn, &p.s)),
                    _ => out.push_str("NULL"),
                }
                out.push_str(seg);
            }
            Ok(out)
        }

        fn last_error(conn: *mut Mysql) -> String {
            conn_err(conn, "exec")
        }

        fn run(conn: *mut Mysql, sql: &str, params: &[DbCell]) -> Result<(), String> {
            let final_sql = build_sql(conn, sql, params)?;
            // SAFETY: `final_sql` outlives the call.
            let r = unsafe {
                mysql_real_query(
                    conn,
                    final_sql.as_ptr() as *const c_char,
                    final_sql.len() as c_ulong,
                )
            };
            if r != 0 {
                return Err(last_error(conn));
            }
            Ok(())
        }

        /// Decode one value by shape (documented wart — see module
        /// note): strict integer shape → int, strict float shape →
        /// float, else text. Shapes admit no whitespace: padding is
        /// data, never skipped.
        fn decode(bytes: &[u8]) -> DbCell {
            if bytes.is_empty() {
                return DbCell::text(String::new());
            }
            let is_int = {
                let digits = match bytes.first() {
                    Some(b'-') if bytes.len() > 1 => &bytes[1..],
                    _ => bytes,
                };
                !digits.is_empty() && digits.iter().all(|b| b.is_ascii_digit())
            };
            if is_int && let Ok(v) = std::str::from_utf8(bytes).unwrap_or("").parse::<i64>() {
                return DbCell::int(v);
            }
            let s = String::from_utf8_lossy(bytes);
            // Float shape: digits with exactly one dot or an
            // exponent marker (no surrounding whitespace).
            let is_float = {
                let t = s.as_ref();
                let t = t.strip_prefix('-').unwrap_or(t);
                let has_digit = t.bytes().any(|b| b.is_ascii_digit());
                let (mant, exp) = match t.find(['e', 'E']) {
                    Some(i) => (&t[..i], Some(&t[i + 1..])),
                    None => (t, None),
                };
                let mant_ok = match mant.find('.') {
                    Some(i) => {
                        let (a, b) = (&mant[..i], &mant[i + 1..]);
                        !a.is_empty()
                            && !b.is_empty()
                            && a.bytes().all(|b| b.is_ascii_digit())
                            && b.bytes().all(|b| b.is_ascii_digit())
                    }
                    None => false,
                };
                let exp_ok = match exp {
                    Some(e) => {
                        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
                        !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
                    }
                    None => false,
                };
                has_digit
                    && (mant_ok
                        || (exp.is_some() && exp_ok && mant.bytes().all(|b| b.is_ascii_digit())))
            };
            if is_float && let Ok(v) = s.parse::<f64>() {
                return DbCell::float(v);
            }
            DbCell::text(s.into_owned())
        }

        pub fn exec(ptr: *mut c_void, sql: &str, params: &[DbCell]) -> Result<i64, String> {
            let conn = ptr as *mut Mysql;
            if sql.trim().is_empty() {
                return Ok(0);
            }
            run(conn, sql, params)?;
            // SAFETY: statement complete; affected-rows is exact for
            // DML, 0 for DDL on this path.
            Ok(unsafe { mysql_affected_rows(conn) } as i64)
        }

        pub fn query(
            ptr: *mut c_void,
            sql: &str,
            params: &[DbCell],
        ) -> Result<Vec<Vec<DbCell>>, String> {
            let conn = ptr as *mut Mysql;
            run(conn, sql, params)?;
            // SAFETY: statement complete. A null result set with
            // zero fields means a row-less statement (callers use
            // `exec` there — empty rows, not an error); a null set
            // WITH fields is a real failure.
            unsafe {
                if mysql_field_count(conn) == 0 {
                    return Ok(Vec::new());
                }
                let res = mysql_store_result(conn);
                if res.is_null() {
                    return Err(last_error(conn));
                }
                let ncols = mysql_num_fields(res) as usize;
                let nrows = mysql_num_rows(res) as usize;
                let mut rows = Vec::with_capacity(nrows);
                loop {
                    let row = mysql_fetch_row(res);
                    if row.is_null() {
                        break;
                    }
                    let lens = mysql_fetch_lengths(res);
                    let mut out = Vec::with_capacity(ncols);
                    for c in 0..ncols {
                        let p = *row.add(c);
                        if p.is_null() {
                            out.push(DbCell::null());
                        } else {
                            let len = *lens.add(c) as usize;
                            let bytes = std::slice::from_raw_parts(p as *const u8, len);
                            out.push(decode(bytes));
                        }
                    }
                    rows.push(out);
                }
                mysql_free_result(res);
                Ok(rows)
            }
        }
    }
}
