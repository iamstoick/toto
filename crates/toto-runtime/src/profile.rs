//! CPU + heap profiling (Enterprise Wave 4, Stage 2): cooperative
//! safepoint sampling with green-thread attribution, plus D1
//! live-object accounting per allocating task.
//!
//! Design (report §Stage 2): no new builtins — env vars +
//! SIGUSR1 + profile file only. Workers sample their own task's
//! `TraceFrame` stack at safepoints (sound, exact attribution);
//! the heap tags every object with its allocating task and the
//! sweep tallies per-task live counts. All cross-thread state is
//! plain data under short-lived locks; the signal handler only
//! writes a pipe byte.

use std::sync::atomic::{AtomicBool, Ordering};

/// Task id meaning "no task" (worker between tasks, drain code,
/// pool/poller threads): allocations and samples attributed here
/// never merge with a live task's numbers.
pub const SYSTEM_TASK: u32 = u32::MAX;

// Per-thread current profiling owner. Set by the worker around
// every task resume (see `worker_loop`), cleared between tasks.
// Read by `Heap::insert` (uniform for MIR + JIT allocators) and
// by the sampler. Pool/poller threads never set it (stays
// `SYSTEM_TASK` there).
thread_local! {
    static CURRENT_OWNER: std::cell::Cell<u32> = const { std::cell::Cell::new(SYSTEM_TASK) };
}

/// Set the allocating/sampling owner for this thread.
pub fn set_owner(task: u32) {
    CURRENT_OWNER.with(|c| c.set(task));
}

/// Current owner for this thread (`SYSTEM_TASK` off-task).
pub fn current_owner() -> u32 {
    CURRENT_OWNER.with(|c| c.get())
}

// Worker index for this thread (`usize::MAX` when unset — never
// sample then). Set once at each `worker_loop` entry; workers are
// pinned OS threads, so it never changes afterwards.
thread_local! {
    static CURRENT_WORKER: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

/// Set this thread's worker index (once per worker thread).
pub fn set_worker(worker: usize) {
    CURRENT_WORKER.with(|c| c.set(worker));
}

/// This thread's worker index, if set.
pub fn current_worker() -> Option<usize> {
    CURRENT_WORKER.with(|c| match c.get() {
        usize::MAX => None,
        w => Some(w),
    })
}

// Per-thread sampling cursor (counter + last-sample instant).
// Lives in TLS so the safepoint fast path takes no locks.
thread_local! {
    static SAMPLE_CURSOR: std::cell::Cell<(u64, u64)> =
        const { std::cell::Cell::new((0, 0)) };
}

/// Safepoint tick: every 1024th call returns true (caller then
/// checks the wall clock). Amortizes the clock read.
pub fn sample_tick() -> bool {
    SAMPLE_CURSOR.with(|c| {
        let (n, t) = c.get();
        let n2 = n.wrapping_add(1);
        c.set((n2, t));
        n2 & 1023 == 0
    })
}

/// Last-sample timestamp (µs since sampling start), for the quantum.
pub fn last_sample() -> u64 {
    SAMPLE_CURSOR.with(|c| c.get().1)
}

/// Stamp the last-sample timestamp (µs since sampling start).
pub fn stamp_sample(now: u64) {
    SAMPLE_CURSOR.with(|c| {
        let (n, _) = c.get();
        c.set((n, now));
    })
}

/// One CPU sample: who ran, when (µs since sampling start), the
/// outermost function (for the task table), and the folded stack
/// (outermost-first, `;`-joined — one allocation per sample).
#[derive(Clone, Debug)]
pub struct Sample {
    pub task: u32,
    pub micros: u64,
    pub first: String,
    pub stack: String,
}

/// Bounded per-worker sample ring (drop-oldest on overflow; the
/// drop counter records loss — growth is never unbounded).
pub struct SampleRing {
    buf: std::collections::VecDeque<Sample>,
    cap: usize,
    pub dropped: u64,
}

impl SampleRing {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::new(),
            cap: cap.max(1),
            dropped: 0,
        }
    }

    pub fn push(&mut self, s: Sample) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
            self.dropped += 1;
        }
        self.buf.push_back(s);
    }

    pub fn drain(&mut self) -> Vec<Sample> {
        std::mem::take(&mut self.buf).into_iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Per-task heap live numbers from one sweep.
#[derive(Clone, Debug, Default)]
pub struct TaskHeap {
    pub objects: usize,
    pub bytes: u64,
}

/// Heap profile: per-task live table + per-type live counts.
#[derive(Clone, Debug, Default)]
pub struct HeapProfile {
    pub tasks: Vec<(u32, TaskHeap)>,
    pub kinds: Vec<(String, usize)>,
}

/// Profiler configuration from the environment (read once at
/// scheduler init; `TOTO_PROFILE=1` enables, `TOTO_PROFILE_HZ`
/// sets the sample rate, `TOTO_PROFILE_OUT` sets the path).
#[derive(Clone, Debug)]
pub struct ProfileConfig {
    pub enabled: bool,
    pub hz: u64,
    pub out: String,
}

impl ProfileConfig {
    pub fn from_env() -> Self {
        let enabled = std::env::var("TOTO_PROFILE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let hz = std::env::var("TOTO_PROFILE_HZ")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|h| *h > 0 && *h <= 10_000)
            .unwrap_or(100);
        let out = std::env::var("TOTO_PROFILE_OUT")
            .unwrap_or_else(|_| format!("toto-profile-{}.txt", std::process::id()));
        Self { enabled, hz, out }
    }
}

// Process-wide profile-trigger self-pipe (unix-only): created
/// once, never closed (2 fds per process — no per-run leak, and
/// the handler's write end can never be recycled under it).
/// The SIGUSR1 handler writes one byte; the enabled run's poller
/// watches the read end.
#[cfg(unix)]
static TRIGGER_PIPE: std::sync::OnceLock<(i32, i32)> = std::sync::OnceLock::new();

/// SIGUSR1 handler: poke the profile pipe, nothing else.
/// Async-signal-safe by construction (single `write(2)`; the
/// write end lives as long as the process).
#[cfg(unix)]
extern "C" fn profile_sigusr1(_sig: std::os::raw::c_int) {
    if let Some((_, w)) = TRIGGER_PIPE.get() {
        let one = [1u8; 1];
        unsafe {
            libc::write(*w, one.as_ptr() as *const std::ffi::c_void, 1);
        }
    }
}

/// Ensure the trigger pipe + handler exist; return the pipe.
/// Unix-only. Idempotent: the first enabled run creates them.
#[cfg(unix)]
pub fn trigger_pipe() -> (i32, i32) {
    *TRIGGER_PIPE.get_or_init(|| {
        // Same startup invariant as the net poller's own pipe
        // (panics on failure — a broken pipe means a broken run).
        let (r, w) = toto_compiler::netsys::self_pipe();
        // `as usize`: the libc `signal(2)` API takes the handler
        // as an integer by definition; the target is an
        // `extern "C"` fn with the C ABI, so the cast is exact.
        #[allow(function_casts_as_integer)]
        unsafe {
            // `signal(2)` (not `sigaction`): restores nothing,
            // needs nothing — the handler is stateless.
            libc::signal(libc::SIGUSR1, profile_sigusr1 as usize);
        }
        (r, w)
    })
}

/// Shared profiler state (lives in `Shared`; see scheduler).
pub struct Profiler {
    pub config: ProfileConfig,
    /// Per-worker rings (only the owning worker pushes; the
    /// dumping worker locks briefly at a safepoint).
    pub rings: Vec<std::sync::Mutex<SampleRing>>,
    /// Set by the poller on SIGUSR1; cleared after a dump.
    pub dump_requested: AtomicBool,
    /// Serializes dumps (first worker to observe the flag wins).
    pub dump_lock: std::sync::Mutex<()>,
    /// Latest sweep tally (written by the collecting worker).
    pub last_heap: std::sync::Mutex<HeapProfile>,
    /// First-seen outermost function per sampled task (for the
    /// task table; tasks have no runtime names of their own).
    pub names: std::sync::Mutex<std::collections::HashMap<u32, String>>,
    /// Sampling start (µs baseline for sample timestamps).
    pub start: std::time::Instant,
}

impl Profiler {
    pub fn new(workers: usize, ring_cap: usize) -> Self {
        let config = ProfileConfig::from_env();
        let mut rings = Vec::with_capacity(workers);
        for _ in 0..workers {
            rings.push(std::sync::Mutex::new(SampleRing::new(ring_cap)));
        }
        Self {
            config,
            rings,
            dump_requested: AtomicBool::new(false),
            dump_lock: std::sync::Mutex::new(()),
            last_heap: std::sync::Mutex::new(HeapProfile::default()),
            names: std::sync::Mutex::new(std::collections::HashMap::new()),
            start: std::time::Instant::now(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn micros_since_start(&self) -> u64 {
        self.start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
    }

    pub fn request_dump(&self) {
        self.dump_requested.store(true, Ordering::Release);
    }

    pub fn take_dump_request(&self) -> bool {
        self.dump_requested.swap(false, Ordering::AcqRel)
    }
}

/// Render the profile file (format §2.3, `profile v1` header).
/// Tasks sorted by id; stacks folded parent-first with counts.
pub fn render(
    samples: &[Sample],
    first_names: &[(u32, String)],
    heap: &HeapProfile,
    dropped: u64,
) -> String {
    let mut out = String::from("profile v1\n");
    let mut by_task: std::collections::BTreeMap<u32, Vec<&Sample>> =
        std::collections::BTreeMap::new();
    for s in samples {
        by_task.entry(s.task).or_default().push(s);
    }
    let names: std::collections::HashMap<u32, &str> = first_names
        .iter()
        .map(|(id, n)| (*id, n.as_str()))
        .collect();
    for (id, ss) in &by_task {
        let nm = names.get(id).copied().unwrap_or("?");
        out.push_str(&format!("task {} {} samples={}\n", id, nm, ss.len()));
    }
    // Folded stacks: same stack shape (joined outermost-first as
    // walked) aggregates a count, per task.
    let mut folds: std::collections::BTreeMap<(u32, String), usize> =
        std::collections::BTreeMap::new();
    for s in samples {
        let key = (s.task, s.stack.clone());
        *folds.entry(key).or_insert(0) += 1;
    }
    for ((id, stack), n) in &folds {
        out.push_str(&format!("stack {} {} {}\n", id, stack, n));
    }
    let mut tasks = heap.tasks.clone();
    tasks.sort_by_key(|(id, _)| *id);
    for (id, t) in &tasks {
        out.push_str(&format!(
            "heap {} objects={} bytes={}\n",
            id, t.objects, t.bytes
        ));
    }
    let mut kinds = heap.kinds.clone();
    kinds.sort();
    for (k, n) in &kinds {
        out.push_str(&format!("kind {} objects={}\n", k, n));
    }
    out.push_str(&format!("dropped {}\n", dropped));
    out
}
