//! Tier-2 ABI gate tests: real MIR → Cranelift → production heap.
//!
//! Each test lowers an inline Toto program through the production
//! pipeline (`parse_and_check` + `mir::lower`), compiles the supported
//! subset, and executes it. Coverage per ADR-008 Tier-2 item is
//! recorded on each test; the panic paths run in the `panic_probe`
//! subprocess (a Toto panic diverges through the context handler,
//! which defaults to spec-§6-on-stderr plus exit 1).

use toto_compiler::{mir, parse_and_check};
use toto_jit::{Jit, JitRt, decode, encode};
use toto_runtime::heap::Payload;

fn lower(src: &str) -> mir::MProgram {
    let (_, checked) = parse_and_check(src).expect("gate program checks");
    mir::lower(&checked)
}

fn compile(src: &str) -> (Jit, JitRt) {
    let prog = lower(src);
    let mut jit = Jit::new().expect("jit session");
    jit.compile(&prog).expect("gate subset compiles");
    (jit, JitRt::new())
}

unsafe fn call0(jit: &Jit, rt: &mut JitRt, name: &str) -> u64 {
    assert!(jit.contains(name), "compiled {name}");
    rt.func = name.to_string();
    unsafe { Jit::call_ptr(jit.ptr(name), rt.ctx_ptr(), &[]) }
}

fn resolve_array(rt: &JitRt, h: u64) -> Vec<toto_runtime::value::RtVal> {
    match rt.heap.get(decode(h)) {
        Some(Payload::Array(elems)) => elems.clone(),
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn gate_value_layout_codec() {
    // Tier-2 item 1 (host side): the ADR-008 codec round-trips, and
    // stale handles resolve to `None` — never to a stranger's object.
    let mut rt = JitRt::new();
    let r = rt
        .heap
        .alloc_array(vec![toto_runtime::value::RtVal::Int(7)]);
    let h = encode(r);
    assert_eq!(decode(h), r);
    // (generation << 32) | index.
    assert_eq!(h, ((r.generation() as u64) << 32) | r.index() as u64);
    assert!(rt.heap.get(decode(h)).is_some());
    // Free the slot (allocate + collect with empty roots after
    // dropping our reference path): the old handle must go stale.
    rt.heap.collect(&[]);
    assert!(rt.heap.get(r).is_none(), "unrooted array reclaimed");
    assert!(
        rt.heap.get(decode(h)).is_none(),
        "stale handle fails cleanly"
    );
    // Immediates traffic as bit patterns.
    assert_eq!(7i64 as u64, 7);
    assert_eq!((-1i64) as u64, u64::MAX);
    assert_eq!(f64::to_bits(0.5), 0x3FE0_0000_0000_0000);
}

#[test]
fn gate_array_layout_and_roots() {
    // Tier-2 items 1 (nested heap layout with descriptor-traced
    // references) and 2 (live handles survive forced collections):
    // real MIR builds `[[1, 2], [3]]`, reads back across indices,
    // with the threshold at 1 so EVERY allocation collects first.
    let src = "fn main() -> int {\n let inner = [1, 2];\n let outer = [inner, [3]];\n return outer[0][1] + outer[1][0];\n}";
    let (jit, mut rt) = compile(src);
    rt.threshold = 1;
    let out = unsafe { call0(&jit, &mut rt, "main") };
    assert_eq!(out as i64, 2 + 3);
    // The collections actually ran (threshold 1 collects before the
    // 2nd and 3rd allocation), and nothing died: every allocated
    // object survived every collection under published roots only.
    let stats = rt.heap.stats();
    assert!(stats.collections >= 2, "collections ran: {stats:?}");
    assert_eq!(stats.reclaimed_total, 0, "nothing reclaimed: {stats:?}");
    // Shadow discipline: epilogue popped everything it pushed.
    assert_eq!(rt.shadow.len(), 0);
}

#[test]
fn gate_nested_layout_host_assert() {
    // Same shape, but return the outer handle through a helper so the
    // test can assert the production-heap layout directly (Tier-2
    // item 1, representation-level: descriptor headers, nested
    // references, lengths).
    let src = "fn make() -> Array<Array<int>> {\n let inner = [1, 2];\n let outer = [inner, [3]];\n return outer;\n}\nfn main() -> int { return 0; }";
    let (jit, mut rt) = compile(src);
    rt.threshold = 1;
    let h = unsafe { call0(&jit, &mut rt, "make") };
    let outer = resolve_array(&rt, h);
    assert_eq!(outer.len(), 2);
    let inner = match &outer[0] {
        toto_runtime::value::RtVal::Heap(r) => resolve_array(&rt, encode(*r)),
        other => panic!("expected nested handle, got {other:?}"),
    };
    let vals: Vec<i64> = inner
        .iter()
        .map(|v| match v {
            toto_runtime::value::RtVal::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        })
        .collect();
    assert_eq!(vals, vec![1, 2]);
    let tail = match &outer[1] {
        toto_runtime::value::RtVal::Heap(r) => resolve_array(&rt, encode(*r)),
        other => panic!("expected nested handle, got {other:?}"),
    };
    assert_eq!(tail.len(), 1);
    // Survival: everything above was allocated under threshold-1
    // collections with only published roots alive.
    let stats = rt.heap.stats();
    assert!(stats.collections >= 2, "collections ran: {stats:?}");
    assert_eq!(stats.reclaimed_total, 0, "nothing reclaimed: {stats:?}");
    assert_eq!(rt.shadow.len(), 0);
}

#[test]
fn gate_missed_root_dies_after_return() {
    // Tier-2 item 2, missed-root control: generated code cannot hold
    // an unpublished value (the compiler auto-publishes every defined
    // handle), so the control runs at the epilogue boundary — the
    // returned handle is unrooted by construction. It resolves while
    // no collection runs, then dies under the next collection and
    // resolves to `None` (never to a stranger's object).
    let src = "fn make() -> Array<int> {\n let a = [10, 20];\n return a;\n}\nfn main() -> int { return 0; }";
    let (jit, mut rt) = compile(src);
    let h = unsafe { call0(&jit, &mut rt, "make") };
    // Live now: resolve and check contents before any collection.
    let elems = resolve_array(&rt, h);
    assert_eq!(elems.len(), 2);
    // Epilogue popped: nothing published anymore.
    assert_eq!(rt.shadow.len(), 0);
    // Collect with empty roots: the unrooted array dies.
    let live_before = rt.heap.stats().objects_live;
    assert!(live_before >= 1);
    let report = rt.heap.collect(&[]);
    assert_eq!(report.live, 0);
    assert!(rt.heap.get(decode(h)).is_none(), "unpublished value died");
}

#[test]
fn gate_jit_on_coroutine_stack() {
    // Tier-2 item 4 (JIT on coroutine stacks, GC roots intact): JIT
    // functions execute on a corosensei coroutine stack; the harness
    // body acts as the runtime around the calls (publish before the
    // suspension point, unpublish after). While suspended, a forced
    // collection keeps the published array alive; the resume value
    // crosses back into JIT code; shadow depth balances across every
    // call in both directions.
    //
    // Yield mechanics below the body (yield from inside live JIT
    // frames) are corosensei's proven domain (Spike 1b proved
    // yield-from-nested-calls for Rust frames; JIT frames are stack
    // frames like any other); lowering yield points inside generated
    // code falls out of park-safepoint lowering, which is Phase 4.
    use corosensei::stack::DefaultStack;
    use corosensei::{Coroutine, CoroutineResult, Yielder};

    let src = "fn wrap_pre() -> Array<int> {\n let a = [0, 41];\n return a;\n}\nfn pick(h: Array<int>, t: int) -> int {\n return h[1] + t;\n}\nfn wrap_post(h: Array<int>, t: int) -> int {\n return h[0] + pick(h, t);\n}\nfn main() -> int { return 0; }";
    let (jit, mut rt) = compile(src);
    for name in ["wrap_pre", "wrap_post", "pick", "main"] {
        rt.entries.insert(name.to_string(), jit.ptr(name) as usize);
    }
    rt.threshold = 1;
    let stack = DefaultStack::new(64 * 1024).expect("coroutine stack");
    let mut co: Coroutine<u64, u64, u64, DefaultStack> =
        Coroutine::with_stack(stack, move |y: &Yielder<u64, u64>, init: u64| -> u64 {
            let rt: &mut JitRt = unsafe { &mut *(init as *mut JitRt) };
            rt.yielder = Some(y as *const Yielder<u64, u64> as *mut Yielder<u64, u64>);
            rt.func = String::from("wrap_pre");
            let make = rt.entry("wrap_pre");
            let h = unsafe { Jit::call_ptr(make, rt.ctx_ptr(), &[]) };
            // Body acts as the runtime: publish the live handle, then
            // suspend. The array must survive collections — and the
            // switch — while suspended.
            rt.shadow.push(decode(h));
            let token = unsafe { toto_jit::runtime::toto_yield(rt.ctx_ptr()) };
            // Resumed: the resume value crosses back into JIT code.
            rt.func = String::from("wrap_post");
            let post = rt.entry("wrap_post");
            let out = unsafe { Jit::call_ptr(post, rt.ctx_ptr(), &[h, token]) };
            rt.shadow.pop_to(0);
            rt.yielder = None;
            out
        });
    match co.resume(rt.ctx_ptr() as u64) {
        CoroutineResult::Yield(tok) => {
            assert_eq!(tok, 0x9e37, "yield token crosses to the scheduler");
            // Suspended: exactly one published root; a forced
            // collection keeps it alive and nothing else.
            assert_eq!(rt.shadow.len(), 1, "published root spans the switch");
            let live = unsafe { toto_jit::runtime::toto_gc_collect(rt.ctx_ptr()) };
            assert_eq!(live, 1, "suspended roots survive collection");
            match co.resume(0x1234) {
                CoroutineResult::Return(out) => {
                    assert_eq!(out as i64, 41 + 0x1234, "h[0] + (h[1] + token)");
                }
                CoroutineResult::Yield(_) => panic!("unexpected second yield"),
            }
        }
        CoroutineResult::Return(_) => panic!("expected the body to yield"),
    }
    // Every prologue/epilogue balanced, both directions.
    assert_eq!(rt.shadow.len(), 0, "shadow depth balanced");
    assert!(rt.yielder.is_none());
    assert!(rt.heap.stats().collections >= 1);
}

fn probe(case: &str) -> (i32, String) {
    let bin = env!("CARGO_BIN_EXE_panic_probe");
    let out = std::process::Command::new(bin)
        .arg(case)
        .output()
        .expect("panic probe runs");
    (
        out.status.code().unwrap_or(-999),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn gate_panics_no_signal() {
    // Tier-2 items 3 (`toto_panic` produces spec-§6 panics) and 5 (no
    // signal escapes generated code: every former SIGFPE-class event
    // is a Toto panic, the process exits 1 — never 136). Spans below
    // are blessed against the source text (each points at the failing
    // operation) and cross-checked with the tree-walking interpreter.
    //
    // `modzero` documents a cosmetic interpreter divergence the gate
    // surfaced: MIR `DivCheck` reports "division by zero" for `%`
    // too (same E0202, same span 39..44); the tree-walker reports
    // "modulo by zero". The spec (§5.2) pins codes, not messages; no
    // corpus program covers `% 0`, so no golden pins either wording.
    for (case, code, msg, span, frame) in [
        ("divzero", "E0202", "division by zero", "41..46", "in main"),
        ("overflow", "E0201", "integer overflow", "57..62", "in main"),
        ("min_div", "E0201", "integer overflow", "62..68", "in main"),
        ("modzero", "E0202", "division by zero", "39..44", "in main"),
        (
            "oob",
            "E0209",
            "index 5 out of bounds for array of length 2",
            "44..48",
            "in main",
        ),
        ("neg", "E0201", "integer overflow", "62..64", "in main"),
        ("stale", "E0204", "stale handle", "37..41", "in f"),
    ] {
        let (exit, err) = probe(case);
        assert_eq!(exit, 1, "{case}: exit code is 1, not a signal");
        assert!(
            err.contains(&format!("panic[{code}]")),
            "{case}: code:\n{err}"
        );
        assert!(err.contains(msg), "{case}: message:\n{err}");
        assert!(err.contains(span), "{case}: span:\n{err}");
        assert!(err.contains("stack trace"), "{case}: trace:\n{err}");
        assert!(err.contains(frame), "{case}: frame:\n{err}");
    }
}

#[test]
fn gate_differential_interpreter() {
    // Mini-differential (Phase-4 precursor): the same programs through
    // the tree-walking interpreter and the JIT must agree on the exit
    // value. Panic-case agreement (codes, spans) is covered by
    // `gate_panics_no_signal` plus the interpreter cross-checks in its
    // comments.
    for (name, src, want) in [
        (
            "arith",
            "fn add3(a: int, b: int) -> int { return a + b * 2; }\nfn main() -> int { return add3(40, 1); }",
            42i64,
        ),
        (
            "div",
            "fn main() -> int {\n let x = 7;\n let y = x / 2;\n return y * 10 + x % 2;\n}",
            3 * 10 + 1,
        ),
        (
            "nested",
            "fn main() -> int {\n let inner = [1, 2];\n let outer = [inner, [3]];\n return outer[0][1] * 100 + outer[1][0];\n}",
            2 * 100 + 3,
        ),
        (
            "negmod",
            "fn main() -> int {\n let a = -7;\n return a / 2 + 10 + a % 2 + 1;\n}",
            -3 + 10 + -1 + 1,
        ),
        (
            "minmod",
            "fn main() -> int {\n let a = -9223372036854775807 - 1;\n return a % -1;\n}",
            0,
        ),
    ] {
        let interp = match toto_compiler::run_source(src, "prog.gzv") {
            toto_compiler::Outcome::Success { code, .. } => code as i64,
            other => panic!("{name}: interpreter failed: {other:?}"),
        };
        assert_eq!(interp, want, "{name}: interpreter golden");
        let (jit, mut rt) = compile(src);
        let out = unsafe { call0(&jit, &mut rt, "main") } as i64;
        assert_eq!(out, want, "{name}: JIT result");
        assert_eq!(rt.shadow.len(), 0, "{name}: shadow balanced");
    }
}

#[test]
fn gate_arith_and_call() {
    // Tier-2 items 1 (int immediates as `i64` words) and 3 (ctx-first
    // calls: generated `main` calls generated `add3`; every call in
    // the chain passes the context first).
    let src = "fn add3(a: int, b: int) -> int { return a + b * 2; }\nfn main() -> int { return add3(40, 1); }";
    let (jit, mut rt) = compile(src);
    let out = unsafe { call0(&jit, &mut rt, "main") };
    assert_eq!(out as i64, 42);
    // Shadow discipline: prologue recorded a base and the epilogue
    // popped to it — no roots leak across the call.
    assert_eq!(rt.shadow.len(), 0);
    assert_eq!(rt.heap.stats().objects_live, 0);
}
