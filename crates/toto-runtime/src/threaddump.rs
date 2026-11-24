//! Green-thread dumps (Enterprise Wave 4, Stage 3): the state of
//! every task in a live or wedged process, for post-mortem
//! diagnosis. Triggered by SIGUSR2 (same self-pipe pattern as the
//! profiler) or automatically on an E0216 deadlock claim. No new
//! builtins: signal + files only, runtime-internal.
//!
//! Format (`threaddump v1`): per-task lines (id, name, state, what
//! it waits on, parked stack), channel waiter lists, pool lanes
//! (queued/in-flight with task + op), worker assignments.

use std::sync::atomic::{AtomicBool, Ordering};

/// SIGUSR2 self-pipe (unix-only): process-wide, created once,
/// never closed (same rationale as the profiler trigger pipe).
#[cfg(unix)]
static THREAD_PIPE: std::sync::OnceLock<(i32, i32)> = std::sync::OnceLock::new();

/// SIGUSR2 handler: poke the thread-dump pipe, nothing else.
/// Async-signal-safe by construction (single `write(2)`).
#[cfg(unix)]
extern "C" fn thread_sigusr2(_sig: std::os::raw::c_int) {
    if let Some((_, w)) = THREAD_PIPE.get() {
        let one = [1u8; 1];
        unsafe {
            libc::write(*w, one.as_ptr() as *const std::ffi::c_void, 1);
        }
    }
}

/// Ensure the trigger pipe + handler exist; return the pipe.
/// Unix-only. Idempotent: the first run needing it creates it.
#[cfg(unix)]
pub fn trigger_pipe() -> (i32, i32) {
    *THREAD_PIPE.get_or_init(|| {
        let (r, w) = toto_compiler::netsys::self_pipe();
        unsafe {
            // `signal(2)` (not `sigaction`): stateless handler.
            #[allow(function_casts_as_integer)]
            libc::signal(libc::SIGUSR2, thread_sigusr2 as usize);
        }
        (r, w)
    })
}

/// Dump flag: set by the poller on SIGUSR2, taken by one worker.
#[derive(Debug, Default)]
pub struct ThreadDumpFlag {
    requested: AtomicBool,
}

impl ThreadDumpFlag {
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    pub fn take(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }
}

/// One channel's dump record: index, cap, closed, buffered depth,
/// sender-waiter task ids, receiver-waiter task ids.
pub type ChannelDump = (usize, usize, bool, usize, Vec<usize>, Vec<usize>);

/// One pool lane's dump record: lane name, queued (task, op text),
/// in-flight count.
pub type PoolDump = (String, Vec<(usize, String)>, u64);

/// One task's dump record (owned plain data — gathering takes
/// only short lock sections, formatting happens lock-free).
#[derive(Clone, Debug)]
pub struct TaskDump {
    pub id: usize,
    pub name: String,
    pub state: String,
    pub wait: String,
    pub scope: String,
    pub stack: Vec<String>,
}

/// Full dump snapshot (owned; rendered lock-free).
#[derive(Clone, Debug, Default)]
pub struct ThreadDump {
    pub tasks: Vec<TaskDump>,
    /// One record per channel (see `ChannelDump`).
    pub channels: Vec<ChannelDump>,
    /// (lane, queued jobs as (task, op text), in_flight count).
    pub pools: Vec<PoolDump>,
    /// (worker, task or idle).
    pub workers: Vec<(usize, Option<usize>)>,
}

/// Render the `threaddump v1` text format.
pub fn render(d: &ThreadDump) -> String {
    let mut out = String::from("threaddump v1\n");
    for t in &d.tasks {
        out.push_str(&format!(
            "task {} {} {} {} scope={}\n",
            t.id, t.name, t.state, t.wait, t.scope
        ));
        for f in &t.stack {
            out.push_str(&format!("  frame {}\n", f));
        }
    }
    for (i, cap, closed, buffered, senders, recvers) in &d.channels {
        out.push_str(&format!(
            "chan {} cap={} closed={} buffered={} send_waiters={:?} recv_waiters={:?}\n",
            i, cap, closed, buffered, senders, recvers
        ));
    }
    for (lane, jobs, flight) in &d.pools {
        out.push_str(&format!(
            "pool {} queued={} in_flight={}\n",
            lane,
            jobs.len(),
            flight
        ));
        for (task, op) in jobs {
            out.push_str(&format!("  job task={} {}\n", task, op));
        }
    }
    for (w, t) in &d.workers {
        match t {
            Some(id) => out.push_str(&format!("worker {} task={}\n", w, id)),
            None => out.push_str(&format!("worker {} idle\n", w)),
        }
    }
    out
}

/// Default dump path (temp dir — never litters the repo),
/// overridable via `TOTO_THREAD_DUMP`.
pub fn default_path() -> String {
    std::env::var("TOTO_THREAD_DUMP").unwrap_or_else(|_| {
        let mut p = std::env::temp_dir();
        p.push(format!("toto-threads-{}.txt", std::process::id()));
        p.display().to_string()
    })
}
