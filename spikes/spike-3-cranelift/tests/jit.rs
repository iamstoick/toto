//! Spike 3 acceptance tests (Section 4 criteria 1-8).
//!
//! Deterministic: hand-built IR, fixed inputs, asserted values and
//! exact GC counts. Timing appears only as printed observations
//! (compile times), never as assertions.

use std::collections::HashMap;
use std::time::Instant;

use spike_1_gc_scheduler::heap::HeapStats;
use spike_3_cranelift::lower::Jit;
use spike_3_cranelift::runtime::JitRuntime;
use spike_3_cranelift::toy::{Expr, FuncId, ToyFunc};

/// Compile one function and call it. Resets the shadow afterwards;
/// roots live only for the duration of the call.
fn run(jit: &Jit, idx: FuncId, rt: &mut JitRuntime, args: &[i64]) -> i64 {
    let base = rt.shadow.len();
    let ptr = jit.ptr(idx);
    let v = unsafe { Jit::call_ptr(ptr, rt.ctx_ptr(), args) };
    rt.shadow.truncate(base);
    v
}

fn define_all(jit: &mut Jit, funcs: &[ToyFunc]) -> Vec<FuncId> {
    let ids: Vec<FuncId> = funcs
        .iter()
        .map(|f| jit.declare(&f.name, f.params))
        .collect();
    for (id, f) in ids.iter().zip(funcs) {
        let t0 = Instant::now();
        jit.define(*id, &f.body);
        eprintln!("jit-compile {}: {} us", f.name, t0.elapsed().as_micros());
    }
    ids
}

fn stats(rt: &JitRuntime) -> HeapStats {
    rt.heap.stats()
}

// --------------------------------- 1. arithmetic, comparison, if, loop

#[test]
fn jit_executes_arithmetic_comparison_if_and_loop() {
    let mut jit = Jit::new().unwrap();
    let mut rt = JitRuntime::new();
    let ids = define_all(
        &mut jit,
        &[
            ToyFunc {
                name: "add".into(),
                params: 2,
                body: Expr::add(Expr::arg(0), Expr::arg(1)),
            },
            ToyFunc {
                name: "sub".into(),
                params: 2,
                body: Expr::sub(Expr::arg(0), Expr::arg(1)),
            },
            ToyFunc {
                name: "mul".into(),
                params: 2,
                body: Expr::mul(Expr::arg(0), Expr::arg(1)),
            },
            ToyFunc {
                name: "eq".into(),
                params: 2,
                body: Expr::eq(Expr::arg(0), Expr::arg(1)),
            },
            ToyFunc {
                name: "lt".into(),
                params: 2,
                body: Expr::lt(Expr::arg(0), Expr::arg(1)),
            },
            ToyFunc {
                name: "max".into(),
                params: 2,
                body: Expr::IfElse {
                    cond: Box::new(Expr::lt(Expr::arg(0), Expr::arg(1))),
                    then_: Box::new(Expr::arg(1)),
                    else_: Box::new(Expr::arg(0)),
                },
            },
            ToyFunc {
                name: "plus100_last".into(),
                params: 1,
                body: Expr::Loop {
                    times: Box::new(Expr::arg(0)),
                    body: Box::new(Expr::add(Expr::Index, Expr::lit(100))),
                },
            },
        ],
    );
    assert_eq!(run(&jit, ids[0], &mut rt, &[3, 4]), 7);
    assert_eq!(run(&jit, ids[1], &mut rt, &[10, 3]), 7);
    assert_eq!(run(&jit, ids[2], &mut rt, &[6, 7]), 42);
    assert_eq!(run(&jit, ids[3], &mut rt, &[5, 5]), 1);
    assert_eq!(run(&jit, ids[3], &mut rt, &[5, 6]), 0);
    assert_eq!(run(&jit, ids[4], &mut rt, &[5, 6]), 1);
    assert_eq!(run(&jit, ids[4], &mut rt, &[6, 5]), 0);
    assert_eq!(run(&jit, ids[5], &mut rt, &[3, 9]), 9);
    assert_eq!(run(&jit, ids[5], &mut rt, &[9, 3]), 9);
    assert_eq!(run(&jit, ids[6], &mut rt, &[5]), 104);
    assert_eq!(run(&jit, ids[6], &mut rt, &[0]), 0);
    // Wrapping (NOT checked): i64::MAX + 1 wraps to MIN.
    let wrap = ToyFunc {
        name: "wrap".into(),
        params: 0,
        body: Expr::add(Expr::lit(i64::MAX), Expr::lit(1)),
    };
    let w = define_all(&mut jit, &[wrap]);
    assert_eq!(run(&jit, w[0], &mut rt, &[]), i64::MIN);
}

// --------------------------------- 2. calls and recursion

#[test]
fn jit_supports_calls_and_recursion() {
    let mut jit = Jit::new().unwrap();
    let mut rt = JitRuntime::new();
    // Declare all first (recursion + cross-calls need this).
    let double = jit.declare("double", 1);
    let quad = jit.declare("quad", 1);
    let fact = jit.declare("fact", 1);
    assert_eq!((double, quad, fact), (0, 1, 2));
    jit.define(double, &Expr::mul(Expr::arg(0), Expr::lit(2)));
    jit.define(
        quad,
        &Expr::call(double, vec![Expr::call(double, vec![Expr::arg(0)])]),
    );
    // fact(n) = n == 0 ? 1 : n * fact(n-1)
    jit.define(
        fact,
        &Expr::IfElse {
            cond: Box::new(Expr::eq(Expr::arg(0), Expr::lit(0))),
            then_: Box::new(Expr::lit(1)),
            else_: Box::new(Expr::mul(
                Expr::arg(0),
                Expr::call(fact, vec![Expr::sub(Expr::arg(0), Expr::lit(1))]),
            )),
        },
    );
    assert_eq!(run(&jit, quad, &mut rt, &[5]), 20);
    assert_eq!(run(&jit, fact, &mut rt, &[0]), 1);
    assert_eq!(run(&jit, fact, &mut rt, &[5]), 120);
    assert_eq!(run(&jit, fact, &mut rt, &[10]), 3628800);
}

// --------------------------------- 3+4. runtime calls + GC integration

/// keep = Keep(AllocInt(x)); garbage = AllocInt(999) [unrooted];
/// collect; read keeper. Returns the keeper's value.
fn keeper_body(garbage: i64) -> Expr {
    Expr::let_(
        1,
        Expr::Keep(Box::new(Expr::AllocInt(Box::new(Expr::arg(0))))),
        Expr::Seq(vec![
            Expr::AllocInt(Box::new(Expr::lit(garbage))),
            Expr::GcCollect,
            Expr::GetIntOr(Box::new(Expr::var(1)), -1),
        ]),
    )
}

#[test]
fn jit_runtime_calls_cross_the_boundary_and_gc_is_exact() {
    let mut jit = Jit::new().unwrap();
    let mut rt = JitRuntime::new();
    let f = jit.declare("keeper", 1);
    jit.define(f, &keeper_body(999));

    let base = rt.shadow.len();
    let v = unsafe { Jit::call_ptr(jit.ptr(f), rt.ctx_ptr(), &[41]) };
    assert_eq!(v, 41, "argument crossed in, keeper value crossed out");
    // Shadow still holds exactly the keeper (no test-side reset yet).
    assert_eq!(rt.shadow.len(), base + 1);

    // The keeper survived (value read back above); exactly one other
    // object was allocated. A manual GC now must find nothing left
    // to reclaim — proving the garbage died in the JIT-triggered
    // collection (no other collector ran: threshold is MAX).
    let st = stats(&rt);
    assert_eq!(st.allocated_objects, 2, "keeper + garbage: {st:?}");
    let rep = rt
        .heap
        .gc_collect_for_test(&rt.shadow.iter().collect::<Vec<_>>());
    assert_eq!(rep.live_after, 1, "only the keeper: {rep:?}");
    assert_eq!(rep.reclaimed_this_run, 0, "garbage died in-JIT: {rep:?}");

    rt.shadow.truncate(base);
    let rep = rt
        .heap
        .gc_collect_for_test(&rt.shadow.iter().collect::<Vec<_>>());
    assert_eq!(rep.live_after, 0);
    assert_eq!(
        rep.reclaimed_this_run, 1,
        "keeper died after unroot: {rep:?}"
    );
}

// --------------------------------- missed-root control in generated code

#[test]
fn jit_missed_root_dies_while_keeper_survives() {
    let mut jit = Jit::new().unwrap();
    let mut rt = JitRuntime::new();
    // temp is computed into a JIT SSA value but never published;
    // keeper is published. After a safepoint the temp must read as
    // the default and the keeper as its value.
    let body = Expr::let_(
        1,
        Expr::Keep(Box::new(Expr::AllocInt(Box::new(Expr::arg(0))))),
        Expr::let_(
            2,
            Expr::AllocInt(Box::new(Expr::lit(777))),
            Expr::Seq(vec![
                Expr::GcCollect,
                Expr::add(
                    Expr::GetIntOr(Box::new(Expr::var(2)), -1000),
                    Expr::GetIntOr(Box::new(Expr::var(1)), -2000),
                ),
            ]),
        ),
    );
    let f = jit.declare("control", 1);
    jit.define(f, &body);
    assert_eq!(run(&jit, f, &mut rt, &[42]), -1000 + 42);
}

// --------------------------------- 5. safepoint inside generated code

#[test]
fn jit_collection_mid_execution_preserves_published_values() {
    let mut jit = Jit::new().unwrap();
    // Small threshold: every few allocations trigger a collection
    // from INSIDE generated code, with the keeper live across it.
    let mut rt = JitRuntime::new().with_threshold(256);
    // keeper = Keep(AllocInt(x)); loop 20x AllocInt (garbage); read keeper.
    let body = Expr::let_(
        1,
        Expr::Keep(Box::new(Expr::AllocInt(Box::new(Expr::arg(0))))),
        Expr::Seq(vec![
            Expr::Loop {
                times: Box::new(Expr::lit(20)),
                body: Box::new(Expr::AllocInt(Box::new(Expr::Index))),
            },
            Expr::GetIntOr(Box::new(Expr::var(1)), -1),
        ]),
    );
    let f = jit.declare("pressure", 1);
    jit.define(f, &body);
    assert_eq!(
        run(&jit, f, &mut rt, &[7]),
        7,
        "keeper intact across safepoints"
    );
    let st = stats(&rt);
    assert!(st.gc_runs >= 1, "threshold fired inside JIT: {st:?}");
    assert!(st.reclaimed_objects >= 1, "garbage died mid-run: {st:?}");
    assert_eq!(st.allocated_objects, 21, "keeper + 20 loop allocs: {st:?}");
}

// --------------------------------- 6. value representation: nested pairs

#[test]
fn jit_pair_objects_round_trip_and_trace_nested_refs() {
    let mut jit = Jit::new().unwrap();
    let mut rt = JitRuntime::new();
    // inner = (1, 2) [UNROOTED: reachable only via outer — this is
    // what proves descriptor tracing, not just direct roots];
    // outer = Keep((inner, 3)); write outer[1] = 99-cell;
    // return GetIntOr(read(outer,0)->read(.,0)) + GetIntOr(read(outer,1)).
    let body = Expr::let_(
        10,
        Expr::AllocPair(
            Box::new(Expr::AllocInt(Box::new(Expr::lit(1)))),
            Box::new(Expr::AllocInt(Box::new(Expr::lit(2)))),
        ),
        Expr::let_(
            11,
            Expr::Keep(Box::new(Expr::AllocPair(
                Box::new(Expr::var(10)),
                Box::new(Expr::AllocInt(Box::new(Expr::lit(3)))),
            ))),
            Expr::let_(
                12,
                Expr::Keep(Box::new(Expr::AllocInt(Box::new(Expr::lit(99))))),
                Expr::Seq(vec![
                    Expr::WriteField(Box::new(Expr::var(11)), 1, Box::new(Expr::var(12))),
                    Expr::GcCollect,
                    Expr::add(
                        Expr::GetIntOr(
                            Box::new(Expr::ReadField(
                                Box::new(Expr::ReadField(Box::new(Expr::var(11)), 0)),
                                0,
                            )),
                            -1,
                        ),
                        Expr::GetIntOr(Box::new(Expr::ReadField(Box::new(Expr::var(11)), 1)), -2),
                    ),
                ]),
            ),
        ),
    );
    let f = jit.declare("pairs", 0);
    jit.define(f, &body);

    // Reachable case: the overwritten int-3 cell is already dead
    // (WriteField ran before the in-JIT collect); 5 live: the two
    // pairs plus the 1, 2 and 99 cells.
    let base = rt.shadow.len();
    let v = unsafe { Jit::call_ptr(jit.ptr(f), rt.ctx_ptr(), &[]) };
    assert_eq!(
        v,
        1 + 99,
        "field writes/reads round-trip through nested pairs"
    );
    let rep = rt
        .heap
        .gc_collect_for_test(&rt.shadow.iter().collect::<Vec<_>>());
    assert_eq!(rep.live_after, 5, "{rep:?}");
    assert_eq!(rep.reclaimed_this_run, 0, "int-3 died in-JIT: {rep:?}");

    // Unreachable case: drop every root, collect, all five die.
    rt.shadow.truncate(base);
    let rep = rt
        .heap
        .gc_collect_for_test(&rt.shadow.iter().collect::<Vec<_>>());
    assert_eq!(rep.live_after, 0, "{rep:?}");
    assert_eq!(rep.reclaimed_this_run, 5, "{rep:?}");
}

// --------------------------------- 7. recompile/replace at toy scale

#[test]
fn jit_recompile_replaces_behavior_for_new_calls() {
    let mut jit = Jit::new().unwrap();
    let mut rt = JitRuntime::new();
    let mut registry: HashMap<String, (*const u8, u64)> = HashMap::new();

    let v1 = jit.declare("f_v1", 1);
    jit.define(v1, &Expr::mul(Expr::arg(0), Expr::lit(2)));
    let p1 = jit.ptr(v1);
    registry.insert("f".into(), (p1, 1));
    let call = |registry: &HashMap<String, (*const u8, u64)>, rt: &mut JitRuntime, x: i64| {
        let (p, _) = registry["f"];
        let base = rt.shadow.len();
        let v = unsafe { Jit::call_ptr(p, rt.ctx_ptr(), &[x]) };
        rt.shadow.truncate(base);
        v
    };
    assert_eq!(call(&registry, &mut rt, 21), 42, "v1 behavior");

    // Recompile under a NEW symbol (cranelift-jit has no per-function
    // replace/unload: the old machine code stays mapped; dispatch
    // switches by registry update — Spike 2's rule at toy scale).
    let v2 = jit.declare("f_v2", 1);
    jit.define(v2, &Expr::mul(Expr::arg(0), Expr::lit(3)));
    let p2 = jit.ptr(v2);
    assert_ne!(p1, p2, "distinct machine code");
    registry.insert("f".into(), (p2, 2));
    assert_eq!(
        call(&registry, &mut rt, 21),
        63,
        "new calls see new behavior"
    );

    // Old pointer retained and still executable (old version alive).
    let base = rt.shadow.len();
    let old = unsafe { Jit::call_ptr(p1, rt.ctx_ptr(), &[21]) };
    rt.shadow.truncate(base);
    assert_eq!(old, 42, "old code still mapped and callable");
}

// --------------------------------- 8. trap behavior (child process)

#[test]
#[cfg(unix)]
fn trap_child_dies() {
    use std::os::unix::process::ExitStatusExt;
    let exe = env!("CARGO_BIN_EXE_trapprobe3");
    let out = std::process::Command::new(exe)
        .output()
        .expect("spawn trap child");
    assert!(!out.status.success(), "trap must not return normally");
    // Observed on Linux x86_64: Cranelift lowers udiv-by-zero to a
    // hardware fault, the process dies with SIGFPE (exit 136).
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGFPE),
        "trap signal, got {:?}",
        out.status.code()
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("calling div10"),
        "child compiled and called: {err}"
    );
}
