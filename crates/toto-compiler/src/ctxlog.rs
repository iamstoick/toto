//! Wave 2 Stage 3 shared runtime state: the monotonic clock, the
//! task-context slot shape, and the structured-log globals.
//!
//! Design notes (ADR-009, implemented exactly):
//! - The clock is monotonic milliseconds since first use (process
//!   start in practice): absolute values compose across fan-out,
//!   relative durations do not (rejected option E). No wall clock in
//!   v1 — timestamps are process-relative, never printed in goldens.
//! - Context values are plain data (`Vec<(String, String)>` plus an
//!   optional absolute deadline). Plain data needs no GC rooting: the
//!   observable contract (immutable library values, spawn
//!   inheritance, scope-failure cancellation, deadline checks at
//!   suspension points) holds either way, and nothing here retains
//!   heap references.
//! - Log configuration (level, sink) and the buffer sink are
//!   process-global: every task logs through the same gate, so a
//!   request handler never needs per-call-site plumbing to reach the
//!   current task's context values.
//! - Both engines (the tree-walking interpreter via `ffi.rs`, the M:N
//!   executor via `exec.rs`) share this module: one clock, one line
//!   format, one escaper. The per-task slot lives per engine
//!   (a thread-local in the interpreter, a scheduler slot field in
//!   the executor); the shape below is shared so the two cannot
//!   drift.
//!
//! Static linking only: pure Rust, no new C symbols, no `dlopen`.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Process clock: the instant of first use. Monotonic by
/// construction (`Instant` never runs backward); millisecond
/// resolution matches the ADR-009 deadline representation.
fn clock_start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

/// Absolute monotonic milliseconds on the shared process clock.
/// Small ints early in the run; never printed in goldens (time
/// passes between scheduling points, so values are nondeterministic
/// by construction).
pub fn monotonic_ms() -> i64 {
    let ms = clock_start().elapsed().as_millis();
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// One task's context slot: ordered pairs plus an optional absolute
/// deadline (`None` = no deadline). `detach_once` serves the next
/// `go` only, then clears: opting out is explicit and one-shot.
#[derive(Clone, Debug, Default)]
pub struct CtxSlot {
    pub pairs: Vec<(String, String)>,
    pub deadline: Option<i64>,
    pub detach_once: bool,
}

impl CtxSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append-or-replace by key, preserving first-insertion order.
    /// Order preservation keeps `current()` round-trips stable; the
    /// log line sorts keys anyway (deterministic regardless).
    pub fn add(&mut self, key: String, val: String) {
        if let Some(slot) = self.pairs.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = val;
        } else {
            self.pairs.push((key, val));
        }
    }

    pub fn clear(&mut self) {
        self.pairs.clear();
        self.deadline = None;
    }
}

// ---------------------------------------------------------------------------
// Structured logging (levels, format, sinks).
// ---------------------------------------------------------------------------

/// Log levels, least to most severe. The int crosses FFI; the name
/// renders in the line. `OFF` (4) disables emission (level gate
/// above every level).
pub const LEVEL_DEBUG: i32 = 0;
pub const LEVEL_INFO: i32 = 1;
pub const LEVEL_WARN: i32 = 2;
pub const LEVEL_ERROR: i32 = 3;
pub const LEVEL_OFF: i32 = 4;

pub fn level_name(level: i32) -> Option<&'static str> {
    match level {
        LEVEL_DEBUG => Some("debug"),
        LEVEL_INFO => Some("info"),
        LEVEL_WARN => Some("warn"),
        LEVEL_ERROR => Some("error"),
        _ => None,
    }
}

/// Sinks. `stdout` flows through the program's captured output (the
/// corpus gate compares it); `stderr` writes the process fd directly;
/// `buffer` retains lines in memory for `drain` (tests, golden
/// pipelines). No file sink in v1: there is no append builtin, and
/// overwriting a file per line is not a log.
pub const SINK_STDOUT: i32 = 0;
pub const SINK_STDERR: i32 = 1;
pub const SINK_BUFFER: i32 = 2;

/// Buffer cap: lines retained before the oldest drops. Test and
/// corpus traffic is hundreds of lines; the cap only bounds runaway
/// loops. Documented, not golden-pinned.
pub const BUFFER_CAP: usize = 16384;

struct LogState {
    level: i32,
    sink: i32,
    buffer: Vec<String>,
    error: Option<String>,
}

fn log_state() -> &'static Mutex<LogState> {
    static STATE: OnceLock<Mutex<LogState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(LogState {
            level: LEVEL_INFO,
            sink: SINK_STDOUT,
            buffer: Vec::new(),
            error: None,
        })
    })
}

fn set_log_error(msg: Option<String>) {
    if let Ok(mut st) = log_state().lock() {
        st.error = msg;
    }
}

pub fn take_log_error() -> String {
    log_state()
        .lock()
        .map(|mut st| st.error.take().unwrap_or_default())
        .unwrap_or_default()
}

/// Set the level gate (`0`–`4`). Returns `0`, or `-1` with the error
/// slot set on an unknown level.
pub fn log_set_level(level: i32) -> i64 {
    if !(LEVEL_DEBUG..=LEVEL_OFF).contains(&level) {
        set_log_error(Some(format!("invalid log level {level} (want 0-3, 4=off)")));
        return -1;
    }
    if let Ok(mut st) = log_state().lock() {
        st.level = level;
        st.error = None;
    }
    0
}

/// Set the sink (`0` stdout, `1` stderr, `2` buffer). Returns `0`,
/// or `-1` with the error slot set on an unknown sink.
pub fn log_set_sink(sink: i32) -> i64 {
    if !(SINK_STDOUT..=SINK_BUFFER).contains(&sink) {
        set_log_error(Some(format!("invalid log sink {sink} (want 0-2)")));
        return -1;
    }
    if let Ok(mut st) = log_state().lock() {
        st.sink = sink;
        st.error = None;
    }
    0
}

/// JSON string escaping for log lines. Escapes `"` and `\`, the
/// short forms for control characters with one (`\n` `\r` `\t`
/// `\b` `\f`), and `\u00xx` (lowercase hex) for the rest below
/// U+0020. Everything else — including DEL and non-ASCII — passes
/// through verbatim (valid JSON over UTF-8 output).
pub fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let v = c as u32;
                out.push_str("\\u00");
                out.push(HEX[((v >> 4) & 0xf) as usize] as char);
                out.push(HEX[(v & 0xf) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out
}

/// One log record. Exact field order (`ts`, `level`, `msg`, `ctx`);
/// context keys sorted so concurrent map orders cannot flake a
/// golden. `ts` is monotonic ms (nondeterministic — golden
/// pipelines strip or avoid it, never pin it).
pub fn format_line(ts: i64, level: &str, msg: &str, pairs: &[(String, String)]) -> String {
    let mut keys: Vec<usize> = (0..pairs.len()).collect();
    keys.sort_by(|&a, &b| pairs[a].0.cmp(&pairs[b].0));
    let mut out = String::new();
    out.push_str("{\"ts\":");
    out.push_str(&ts.to_string());
    out.push_str(",\"level\":\"");
    out.push_str(level);
    out.push_str("\",\"msg\":\"");
    out.push_str(&escape_json(msg));
    out.push_str("\",\"ctx\":{");
    for (i, &k) in keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&escape_json(&pairs[k].0));
        out.push_str("\":\"");
        out.push_str(&escape_json(&pairs[k].1));
        out.push('"');
    }
    out.push_str("}}");
    out
}

/// Emit one record through the current level gate and sink.
/// Returns `1` when a line was produced, `0` when the level gate
/// filtered it. `stdout` lines return in `line_out` for the caller
/// to append to captured output; `stderr` writes the fd directly
/// (one `write` per line); `buffer` retains for `drain`.
pub fn log_emit(level: i32, msg: &str, pairs: &[(String, String)]) -> (i64, Option<String>) {
    let Some(name) = level_name(level) else {
        set_log_error(Some(format!("invalid log level {level} (want 0-3)")));
        return (-1, None);
    };
    let (gate, sink) = match log_state().lock() {
        Ok(st) => (st.level, st.sink),
        Err(_) => return (-1, None),
    };
    if level < gate {
        return (0, None);
    }
    let line = format_line(monotonic_ms(), name, msg, pairs);
    match sink {
        SINK_STDOUT => (1, Some(line)),
        SINK_STDERR => {
            use std::io::Write as _;
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{line}");
            (1, None)
        }
        _ => {
            if let Ok(mut st) = log_state().lock() {
                if st.buffer.len() >= BUFFER_CAP {
                    st.buffer.remove(0);
                }
                st.buffer.push(line);
            }
            (1, None)
        }
    }
}

/// Drain the buffer sink: lines joined with `\n` (no trailing
/// newline; lines never contain raw newlines — `msg` escaping
/// guarantees the split is exact), clearing the buffer. Empty when
/// nothing was buffered.
pub fn log_drain() -> String {
    log_state()
        .lock()
        .map(|mut st| {
            let out = st.buffer.join("\n");
            st.buffer.clear();
            out
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_json_cases() {
        assert_eq!(escape_json("plain"), "plain");
        assert_eq!(escape_json("a\"b"), "a\\\"b");
        assert_eq!(escape_json("a\\b"), "a\\\\b");
        assert_eq!(escape_json("a\nb\tc\rd"), "a\\nb\\tc\\rd");
        assert_eq!(escape_json("a\x08b\x0cc"), "a\\bb\\fc");
        assert_eq!(escape_json("a\x01b\x7f"), "a\\u0001b\x7f");
        // DEL passes through (valid JSON); non-ASCII passes through.
        assert_eq!(escape_json("caf\u{e9}"), "caf\u{e9}");
    }

    #[test]
    fn format_line_orders_and_sorts() {
        let pairs = vec![
            ("trace_id".to_string(), "req-1".to_string()),
            ("a".to_string(), "x\"y".to_string()),
        ];
        let line = format_line(7, "info", "m", &pairs);
        assert_eq!(
            line,
            "{\"ts\":7,\"level\":\"info\",\"msg\":\"m\",\"ctx\":{\"a\":\"x\\\"y\",\"trace_id\":\"req-1\"}}"
        );
        let empty: Vec<(String, String)> = Vec::new();
        assert_eq!(
            format_line(0, "error", "e", &empty),
            "{\"ts\":0,\"level\":\"error\",\"msg\":\"e\",\"ctx\":{}}"
        );
    }

    // One stateful test (process-global gate + buffer): sequential
    // within itself, and no other test in this binary touches the
    // log globals, so parallel harnesses cannot interfere. Covers
    // validation, gating, sinks, and drain in one thread.
    #[test]
    fn log_state_gate_and_validation() {
        // Invalid inputs fail with the slot set; valid inputs clear.
        assert_eq!(log_set_level(9), -1);
        assert_eq!(take_log_error(), "invalid log level 9 (want 0-3, 4=off)");
        assert_eq!(log_set_sink(-1), -1);
        assert_eq!(take_log_error(), "invalid log sink -1 (want 0-2)");
        assert_eq!(log_set_level(LEVEL_WARN), 0);
        assert_eq!(take_log_error(), "");
        assert_eq!(log_set_sink(SINK_BUFFER), 0);
        assert_eq!(take_log_error(), "");
        let _ = log_drain();
        let pairs = vec![("trace_id".to_string(), "r".to_string())];
        // Filtered: no line, return 0.
        let (rc, line) = log_emit(LEVEL_INFO, "hidden", &pairs);
        assert_eq!(rc, 0);
        assert!(line.is_none());
        // Admitted to buffer: return 1, no stdout line (the line
        // lives in the buffer — the Toto wrapper prints only the
        // stdout sink's lines).
        let (rc, line) = log_emit(LEVEL_ERROR, "shown", &pairs);
        assert_eq!(rc, 1);
        assert!(line.is_none());
        let drained = log_drain();
        assert!(drained.contains("\"level\":\"error\""), "{drained}");
        assert!(drained.contains("\"trace_id\":\"r\""), "{drained}");
        assert_eq!(log_drain(), "");
        // Invalid level: loud, no line.
        let (rc, line) = log_emit(77, "bad", &pairs);
        assert_eq!(rc, -1);
        assert!(line.is_none());
        assert_eq!(take_log_error(), "invalid log level 77 (want 0-3)");
        // Restore the defaults for any later user of the globals.
        log_set_level(LEVEL_INFO);
        log_set_sink(SINK_STDOUT);
        let _ = log_drain();
    }
}
