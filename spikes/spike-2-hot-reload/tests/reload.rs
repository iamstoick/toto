//! Spike 2 acceptance tests (Section 4 criteria 1-7).
//!
//! Deterministic except for the drain-timeout path, which waits out
//! a SHORT test timeout (100 ms, never the provisional 5 s dev-loop
//! default). Timeout tests assert outcomes, not durations.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use spike_2_hot_reload::{ChangeKind, EntryFn, Func, FuncId, Hctx, HotRuntime, Outcome};

const DOUBLE: FuncId = 1;
const STACK: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_millis(100);

fn v1_funcs() -> HashMap<FuncId, Func> {
    let mut m = HashMap::new();
    m.insert(
        DOUBLE,
        Rc::new(|h: &Hctx, args: &[i64]| {
            h.log(format!("v1 double {}", args[0]));
            args[0] * 2
        }) as Func,
    );
    m
}

fn v2_funcs() -> HashMap<FuncId, Func> {
    let mut m = HashMap::new();
    m.insert(
        DOUBLE,
        Rc::new(|h: &Hctx, args: &[i64]| {
            h.log(format!("v2 double {}", args[0]));
            args[0] * 3
        }) as Func,
    );
    m
}

fn install_v1(rt: &mut HotRuntime) -> u64 {
    rt.install_version(v1_funcs(), HashMap::new())
}

/// Pump until `old` has no frames left (cap: fail loudly, never hang).
fn drain_frames(rt: &mut HotRuntime, old: u64) {
    for _ in 0..10_000 {
        if rt.quiescent(old) {
            return;
        }
        assert!(rt.pump_once(), "drain stalled with tasks blocked");
    }
    panic!("drain did not converge");
}

// --------------------------------- 1+2. body drain, versions, quiescence

fn slow_body(h: Hctx) {
    let res = h.in_scope(|h, s| {
        for i in 0..4 {
            let v = h.call(s, DOUBLE, &[i]);
            h.log(format!("slow used {v}"));
            if h.yield_point().is_err() {
                return;
            }
        }
    });
    match res {
        Ok(()) => h.log("slow done"),
        Err(_) => h.log("slow cancelled"),
    }
}

fn quick_body(h: Hctx) {
    let res = h.in_scope(|h, s| {
        let v = h.call(s, DOUBLE, &[10]);
        h.log(format!("quick used {v}"));
    });
    match res {
        Ok(()) => h.log("quick done"),
        Err(_) => h.log("quick cancelled"),
    }
}

#[test]
fn body_change_drains_old_version_and_new_scopes_use_new() {
    let mut rt = HotRuntime::new();
    let v1 = install_v1(&mut rt);
    let slow: EntryFn = Rc::new(slow_body);
    let s = rt.spawn_scope("slow", STACK, Rc::clone(&slow));

    // Slow task enters its v1 scope and suspends inside it.
    rt.drive_once();
    rt.drive_once();
    assert!(!rt.quiescent(v1), "slow frame still on v1");

    let pending = rt.begin_reload(ChangeKind::Body, v2_funcs(), HashMap::new());
    assert_eq!(pending.old, v1);

    // New scopes bind the new version immediately...
    let q = rt.spawn_scope("quick", STACK, Rc::new(quick_body));
    rt.join_scope(q);
    assert!(rt.is_done_scope(q));

    // ...while the old version is NOT retired mid-drain.
    rt.drive_once();
    assert!(!rt.quiescent(v1), "slow frame still draining");
    assert!(!rt.is_retired(v1), "old version retired early");

    // Drain to completion, then retire.
    drain_frames(&mut rt, v1);
    assert!(rt.retire_if_quiescent(v1));
    assert!(rt.is_retired(v1));
    rt.join_scope(s);

    let log = rt.event_log().join("\n");
    // Old frame finished on old code: 0,2,4,6.
    for (i, v) in [0, 2, 4, 6].iter().enumerate() {
        assert!(log.contains(&format!("slow used {v}")), "round {i}: {log}");
    }
    assert!(log.contains("slow done"), "{log}");
    // New scope ran new code exactly once.
    assert!(log.contains("quick used 30"), "{log}");
    assert!(log.contains("quick done"), "{log}");
    assert!(!log.contains("slow cancelled"), "{log}");

    let rlog = rt.reload_log().join("\n");
    assert!(rlog.contains("draining"), "{rlog}");
    assert!(rlog.contains(&format!("retired v{v1}")), "{rlog}");

    // GC sanity: both version tokens live, nothing else allocated.
    let rep = rt.gc_for_test();
    assert_eq!(rep.live_after, 2, "{rep:?}");
    assert_eq!(rep.reclaimed_this_run, 0, "{rep:?}");
}

// --------------------------------- 3. timeout + cancel + respawn

fn looper_entry(generation: Rc<Cell<u32>>) -> EntryFn {
    Rc::new(move |h: Hctx| {
        let g = generation.get();
        generation.set(g + 1);
        if g == 0 {
            // Generation 0 never drains: loops forever in its scope.
            let res = h.in_scope(|h, s| {
                loop {
                    let v = h.call(s, DOUBLE, &[1]);
                    assert_eq!(v, 2, "gen0 runs old code");
                    if h.yield_point().is_err() {
                        return;
                    }
                }
            });
            match res {
                Ok(()) => h.log("gen0 done?!"),
                Err(_) => h.log("gen0 cancelled at boundary"),
            }
        } else {
            // Respawned generation: finite, on the new version.
            let res = h.in_scope(|h, s| {
                let v = h.call(s, DOUBLE, &[7]);
                h.log(format!("gen1 used {v}"));
            });
            match res {
                Ok(()) => h.log("gen1 done"),
                Err(_) => h.log("gen1 cancelled?!"),
            }
        }
    })
}

#[test]
fn drain_timeout_cancels_at_boundary_and_respawns_clean() {
    let mut rt = HotRuntime::new();
    let v1 = install_v1(&mut rt);
    let generation = Rc::new(Cell::new(0u32));
    let s = rt.spawn_scope("looper", STACK, looper_entry(Rc::clone(&generation)));
    rt.drive_once();
    rt.drive_once();

    let rep = rt.reload_for_test(ChangeKind::Body, v2_funcs(), HashMap::new(), TIMEOUT);
    assert_eq!(rep.old, v1);
    assert!(
        matches!(rep.outcome, Outcome::CancelledAndRespawned(1)),
        "{rep:?}"
    );

    // Respawned generation runs to completion on the new version.
    rt.join_scope(s);
    assert!(rt.is_done_scope(s));

    let log = rt.event_log().join("\n");
    assert!(log.contains("gen0 cancelled at boundary"), "{log}");
    assert!(!log.contains("gen0 done"), "{log}");
    assert!(log.contains("gen1 used 21"), "{log}");
    assert!(log.contains("gen1 done"), "{log}");
    // No partial state: generation 1 started fresh (input 7 -> 21),
    // never resumed mid-loop.
    let rlog = rt.reload_log().join("\n");
    assert!(rlog.contains(&format!("cancelled v{v1}")), "{rlog}");
    assert!(rlog.contains("respawned looper"), "{rlog}");
    assert!(rlog.contains(&format!("retired v{v1}")), "{rlog}");

    // Retired version collectable: only the current token survives.
    rt.drive_once();
    rt.drive_once();
    let gc = rt.gc_for_test();
    assert_eq!(gc.live_after, 1, "{gc:?}");
}

// --------------------------------- 4+5. matrix + restart freshness

#[test]
fn reloadability_matrix_classification() {
    use spike_2_hot_reload::{Disposition, classify};
    assert_eq!(classify(ChangeKind::Body), Disposition::DrainReload);
    assert_eq!(classify(ChangeKind::Signature), Disposition::ScopeRestart);
    assert_eq!(classify(ChangeKind::StructAdd), Disposition::FullRestart);
    assert_eq!(classify(ChangeKind::StructRemove), Disposition::FullRestart);
    assert_eq!(classify(ChangeKind::ChannelType), Disposition::FullRestart);
    assert_eq!(classify(ChangeKind::GlobalInit), Disposition::FullRestart);
}

fn add2_funcs() -> HashMap<FuncId, Func> {
    let mut m = HashMap::new();
    m.insert(
        DOUBLE,
        Rc::new(|h: &Hctx, args: &[i64]| {
            h.log("v1 add2");
            args[0] + args[1]
        }) as Func,
    );
    m
}

fn add3_funcs() -> HashMap<FuncId, Func> {
    let mut m = HashMap::new();
    m.insert(
        DOUBLE,
        Rc::new(|h: &Hctx, args: &[i64]| {
            h.log("v2 add3");
            args[0] + args[1] + args[2]
        }) as Func,
    );
    m
}

fn sig_entry(generation: Rc<Cell<u32>>) -> EntryFn {
    Rc::new(move |h: Hctx| {
        let g = generation.get();
        generation.set(g + 1);
        if g == 0 {
            // Old scope: run old arity, then suspend inside it. The
            // new arity can never run here; cancel must land first.
            let res = h.in_scope(|h, s| {
                let v = h.call(s, DOUBLE, &[1, 2]);
                h.log(format!("sig old used {v}"));
                if h.yield_point().is_err() {
                    return;
                }
                h.log("sig old resumed?!");
            });
            match res {
                Ok(()) => h.log("sig old done?!"),
                Err(_) => h.log("sig old cancelled"),
            }
        } else {
            let res = h.in_scope(|h, s| {
                let v = h.call(s, DOUBLE, &[1, 2, 3]);
                h.log(format!("sig used {v}"));
            });
            match res {
                Ok(()) => h.log("sig done"),
                Err(_) => h.log("sig cancelled?!"),
            }
        }
    })
}

#[test]
fn signature_change_restarts_scope() {
    let mut rt = HotRuntime::new();
    rt.install_version(add2_funcs(), HashMap::new());
    let generation = Rc::new(Cell::new(0u32));
    let s = rt.spawn_scope("sig", STACK, sig_entry(Rc::clone(&generation)));
    rt.drive_once();
    rt.drive_once(); // old scope entered, suspended inside
    // Old scope cannot serve the new arity: restart, don't drain.
    let rep = rt.reload_for_test(ChangeKind::Signature, add3_funcs(), HashMap::new(), TIMEOUT);
    assert!(matches!(rep.outcome, Outcome::ScopeRestarted(1)), "{rep:?}");
    rt.join_scope(s);
    let log = rt.event_log().join("\n");
    assert!(log.contains("sig old used 3"), "{log}");
    assert!(log.contains("sig old cancelled"), "{log}");
    assert!(!log.contains("sig old resumed"), "{log}");
    assert!(log.contains("sig used 6"), "{log}");
    assert!(log.contains("sig done"), "{log}");
}

#[test]
fn layout_typed_changes_restart_fresh() {
    for change in [
        ChangeKind::StructAdd,
        ChangeKind::StructRemove,
        ChangeKind::ChannelType,
    ] {
        let mut rt = HotRuntime::new();
        rt.install_version(v1_funcs(), HashMap::from([("mode".to_owned(), 1)]));
        rt.set_global("mode", 1);
        let (_tag, _ch) = rt.channel("int");
        let rep = rt.reload_for_test(
            change,
            v2_funcs(),
            HashMap::from([("mode".to_owned(), 2)]),
            TIMEOUT,
        );
        assert!(
            matches!(rep.outcome, Outcome::Restarted),
            "{change:?}: {rep:?}"
        );
        // Fresh process state: holder only, empty heap, new globals.
        assert_eq!(rt.task_count(), 1, "holder only");
        assert_eq!(rt.heap_stats().allocated_objects, 0);
        assert_eq!(rt.get_global("mode"), 2, "new initializers applied");
        assert_eq!(rt.current_version(), rep.new);
    }
}

#[test]
fn global_initializer_change_restarts_with_new_initializers() {
    let mut rt = HotRuntime::new();
    rt.install_version(v1_funcs(), HashMap::from([("mode".to_owned(), 1)]));
    rt.set_global("mode", 1);
    assert_eq!(rt.get_global("mode"), 1);
    let rep = rt.reload_for_test(
        ChangeKind::GlobalInit,
        v2_funcs(),
        HashMap::from([("mode".to_owned(), 2)]),
        TIMEOUT,
    );
    assert!(matches!(rep.outcome, Outcome::Restarted), "{rep:?}");
    assert_eq!(rt.get_global("mode"), 2);
    assert_eq!(rt.task_count(), 1);
}

fn struct_user(h: Hctx) {
    let res = h.in_scope(|h, _s| {
        // Old-layout data (tag 1, two fields) under a new-layout
        // reader (tag 2): must be detected, never reinterpreted.
        let obj = h.alloc_struct(1, &[10, 20]);
        match h.read_struct(obj, 2, 3) {
            Ok(_) => h.log("reinterpreted?!"),
            Err(e) => h.log(format!("mismatch routed: have {} want {}", e.have, e.want)),
        }
    });
    match res {
        Ok(()) => h.log("struct probe done"),
        Err(_) => h.log("struct probe cancelled"),
    }
}

#[test]
fn struct_layout_mismatch_never_reinterprets() {
    let mut rt = HotRuntime::new();
    install_v1(&mut rt);
    let s = rt.spawn_scope("probe", STACK, Rc::new(struct_user));
    rt.join_scope(s);
    let log = rt.event_log().join("\n");
    assert!(log.contains("mismatch routed: have 1 want 2"), "{log}");
    assert!(!log.contains("reinterpreted"), "{log}");
    assert!(log.contains("struct probe done"), "{log}");
}

// --------------------------------- 6. callable lifetime

fn callable_holder(h: Hctx) {
    let c = h
        .in_scope(|h, s| {
            let c = h.make_callable(s, DOUBLE);
            let v = h.call_callable(c, &[21]);
            h.log(format!("callable gave {v}"));
            c
        })
        .expect("scope runs");
    // Scope exited (frames == 0) but the callable is still held.
    h.log("holding");
    let _ = h.yield_point();
    // Invoked again after the reload below: still old behavior.
    let v = h.call_callable(c, &[21]);
    h.log(format!("callable gave {v}"));
    let _ = h.yield_point();
    h.log("dropping");
    h.drop_callable(c);
    h.log("dropped");
}

#[test]
fn version_retained_while_callable_held_then_reclaimable() {
    let mut rt = HotRuntime::new();
    let v1 = install_v1(&mut rt);
    let h = rt.spawn_scope("holder", STACK, Rc::new(callable_holder));

    rt.drive_once(); // holder syncs version token
    rt.drive_once(); // holder creates callable, invokes (42), suspends holding
    assert_eq!(rt.version_frames(v1), 0, "scope exited, frames gone");

    let pending = rt.begin_reload(ChangeKind::Body, v2_funcs(), HashMap::new());
    assert_eq!(pending.old, v1);
    rt.drive_once(); // holder syncs v2 token
    rt.drive_once(); // holder resumes: second invoke still v1 behavior
    assert!(rt.quiescent(v1), "no frames, yet...");
    assert!(!rt.is_retired(v1), "callable retention blocks retirement");
    assert_eq!(rt.callables_of(v1), 1);

    // Forced GC preserves everything reachable: both tokens, the
    // callable pair, its func-id cell. Nothing else allocated.
    let gc = rt.gc_for_test();
    assert_eq!(gc.live_after, 4, "{gc:?}");
    assert_eq!(gc.reclaimed_this_run, 0, "{gc:?}");

    rt.join_scope(h); // dropping, dropped
    assert!(
        rt.retire_if_quiescent(v1),
        "released + quiescent => retired"
    );
    rt.drive_once(); // holder sync unroots the retired token
    rt.drive_once();
    let gc = rt.gc_for_test();
    assert_eq!(gc.live_after, 1, "only the current token: {gc:?}");
    assert_eq!(gc.reclaimed_this_run, 3, "{gc:?}");

    let log = rt.event_log().join("\n");
    assert_eq!(log.matches("callable gave 42").count(), 2, "{log}");
    assert!(log.contains("dropped"), "{log}");
}

// --------------------------------- 7a. reload during suspension: drain

fn suspender(h: Hctx) {
    let res = h.in_scope(|h, s| {
        let v = h.call(s, DOUBLE, &[5]);
        h.log(format!("suspender computed {v}"));
        let _ = h.yield_point(); // suspended mid-scope on old code
        let w = h.call(s, DOUBLE, &[6]);
        h.log(format!("suspender resumed {w}"));
    });
    match res {
        Ok(()) => h.log("suspender done"),
        Err(_) => h.log("suspender cancelled"),
    }
}

#[test]
fn suspended_task_resumes_on_old_version_and_drains() {
    let mut rt = HotRuntime::new();
    let v1 = install_v1(&mut rt);
    let s = rt.spawn_scope("susp", STACK, Rc::new(suspender));

    rt.drive_once(); // holder syncs token
    rt.drive_once(); // suspender runs to its mid-scope yield
    assert!(!rt.quiescent(v1), "suspended task counts as a live frame");
    assert_eq!(rt.version_frames(v1), 1);

    let pending = rt.begin_reload(ChangeKind::Body, v2_funcs(), HashMap::new());
    assert_eq!(pending.old, v1);

    // Forced GC mid-suspension: only the current token is live
    // (suspender allocated nothing); nothing reclaimed.
    let gc = rt.gc_for_test();
    assert_eq!(gc.live_after, 1, "{gc:?}");
    assert_eq!(gc.reclaimed_this_run, 0, "{gc:?}");

    // Resume: still old code, no mixing, then drain + retire.
    drain_frames(&mut rt, v1);
    assert!(rt.retire_if_quiescent(v1));
    rt.join_scope(s);

    let log = rt.event_log().join("\n");
    assert_eq!(
        log.lines()
            .filter(|l| l.contains("suspender"))
            .collect::<Vec<_>>(),
        [
            "suspender computed 10",
            "suspender resumed 12",
            "suspender done"
        ],
    );
}

// --------------------------------- 7b. reload during suspension: timeout

fn stuck_body(h: Hctx) {
    let res = h.in_scope(|h, s| {
        loop {
            let v = h.call(s, DOUBLE, &[1]);
            assert_eq!(v, 2, "stuck scope runs old code only");
            h.log("stuck tick");
            if h.yield_point().is_err() {
                return;
            }
        }
    });
    match res {
        Ok(()) => h.log("stuck done?!"),
        Err(_) => h.log("stuck cancelled at boundary"),
    }
}

fn finite_body(h: Hctx) {
    let res = h.in_scope(|h, s| {
        let v = h.call(s, DOUBLE, &[7]);
        h.log(format!("finite used {v}"));
    });
    match res {
        Ok(()) => h.log("finite done"),
        Err(_) => h.log("finite cancelled?!"),
    }
}

fn stuck_entry(generation: Rc<Cell<u32>>) -> EntryFn {
    Rc::new(move |h: Hctx| {
        let g = generation.get();
        generation.set(g + 1);
        if g == 0 {
            stuck_body(h);
        } else {
            finite_body(h);
        }
    })
}

#[test]
fn suspended_task_timeout_cancels_at_boundary_no_mixing() {
    let mut rt = HotRuntime::new();
    let v1 = install_v1(&mut rt);
    let generation = Rc::new(Cell::new(0u32));
    let s = rt.spawn_scope("stuck", STACK, stuck_entry(Rc::clone(&generation)));
    rt.drive_once();
    rt.drive_once();

    let rep = rt.reload_for_test(ChangeKind::Body, v2_funcs(), HashMap::new(), TIMEOUT);
    assert!(
        matches!(rep.outcome, Outcome::CancelledAndRespawned(1)),
        "{rep:?}"
    );
    assert!(rt.is_retired(v1));
    rt.join_scope(s);

    let log = rt.event_log().join("\n");
    assert!(log.contains("stuck cancelled at boundary"), "{log}");
    assert!(!log.contains("stuck done"), "{log}");
    // Respawned generation runs new code only.
    assert!(log.contains("finite used 21"), "{log}");
    assert!(log.contains("finite done"), "{log}");
    // No mixed-version frame: the stuck scope never saw v2, the
    // finite scope never saw v1.
    assert!(!log.contains("stuck used 3"), "{log}");
}
