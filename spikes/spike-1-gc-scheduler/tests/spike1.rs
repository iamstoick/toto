//! Spike 1 acceptance tests (Section 5 criteria 1-5).
//!
//! Deterministic: round-robin scheduler, no randomness, no timing
//! assertions. GC behavior is asserted through handles and the event
//! log, never through wall-clock time.

use spike_1_gc_scheduler::{Action, Cx, GcRef, Scheduler, Step};

// ---------------------------------------------------------------- Task kit

/// Ticks `left` times, then finishes. Logs every step.
struct Tick {
    name: &'static str,
    left: u32,
    steps: u32,
}

impl Step for Tick {
    fn step(&mut self, cx: &mut Cx) -> Action {
        if self.left == 0 {
            cx.log(format!("{} done steps={}", self.name, self.steps));
            return Action::Done;
        }
        self.steps += 1;
        cx.log(format!("{} tick {}", self.name, self.steps));
        self.left -= 1;
        Action::Yield
    }
}

/// cons cell built in a nested frame. Returns unrooted: the caller must
/// root it before the next safepoint (allocation or step return).
fn cons(cx: &mut Cx, head_val: i64, tail: GcRef) -> GcRef {
    cx.with_frame(|cx| {
        let h = cx.alloc_rooted_int(head_val);
        cx.alloc_pair(h, tail)
    })
}

// ------------------------------------------------- 1. yield/join/predictable

#[test]
fn green_tasks_yield_resume_and_join_predictably() {
    let mut s = Scheduler::new();
    let a = s.spawn(
        "a",
        Box::new(Tick {
            name: "a",
            left: 2,
            steps: 0,
        }),
    );
    let b = s.spawn(
        "b",
        Box::new(Tick {
            name: "b",
            left: 2,
            steps: 0,
        }),
    );
    let c = s.spawn(
        "c",
        Box::new(Tick {
            name: "c",
            left: 2,
            steps: 0,
        }),
    );

    s.join(a);
    assert!(s.is_done(a));
    s.join(b);
    s.join(c);
    // Joining again is a no-op, not a re-run.
    s.join(a);

    let log: Vec<&str> = s.event_log().iter().map(String::as_str).collect();
    assert_eq!(
        log,
        [
            "a tick 1",
            "b tick 1",
            "c tick 1",
            "a tick 2",
            "b tick 2",
            "c tick 2",
            "a done steps=2",
            "b done steps=2",
            "c done steps=2",
        ]
    );
    assert_eq!(s.task_count(), 3);
}

// --------------------------------- 2. nested roots + switches + reclamation

struct Builder {
    phase: u8,
    list: Option<GcRef>,
}

impl Step for Builder {
    fn step(&mut self, cx: &mut Cx) -> Action {
        match self.phase {
            0 => {
                // Build (10, (20, (30, nil))) with one frame per cons.
                let list = cx.with_frame(|cx| {
                    let nil = cx.alloc_rooted_int(0);
                    let mut list = nil;
                    for v in [30, 20, 10] {
                        let cell = cons(cx, v, list);
                        cx.root(cell);
                        list = cell;
                    }
                    list
                });
                cx.root(list);
                self.list = Some(list);
                let depth = cx.shadow_len();
                cx.log(format!("built depth={depth}"));
                self.phase = 1;
                Action::Yield
            }
            1 | 2 => {
                let list = self.list.expect("list kept across the switch");
                assert!(cx.shadow_contains(list), "list root survived switch");
                // Explicit walk through descriptors: (10, (20, (30, 0))).
                let (h10, t10) = cx.heap_get_pair(list).expect("pair survives");
                assert_eq!(cx.heap_get_int(h10), Some(10));
                let (h20, t20) = cx.heap_get_pair(t10).expect("pair survives");
                assert_eq!(cx.heap_get_int(h20), Some(20));
                let (h30, tnil) = cx.heap_get_pair(t20).expect("pair survives");
                assert_eq!(cx.heap_get_int(h30), Some(30));
                assert_eq!(cx.heap_get_int(tnil), Some(0));
                cx.log(format!("verified phase={}", self.phase));
                self.phase += 1;
                Action::Yield
            }
            _ => {
                cx.log("builder done");
                Action::Done
            }
        }
    }
}

struct Churn {
    rounds: u8,
}

impl Step for Churn {
    fn step(&mut self, cx: &mut Cx) -> Action {
        if self.rounds == 0 {
            cx.log("churn done");
            return Action::Done;
        }
        for i in 0..40 {
            // Deliberately unrooted: pure garbage for the next GC.
            cx.alloc_int(i);
        }
        let rep = cx.gc_collect_for_test();
        cx.log(format!("churn reclaimed={}", rep.reclaimed_this_run));
        self.rounds -= 1;
        Action::Yield
    }
}

#[test]
fn live_objects_survive_nested_calls_switches_and_forced_gc() {
    let mut s = Scheduler::new();
    let b = s.spawn(
        "builder",
        Box::new(Builder {
            phase: 0,
            list: None,
        }),
    );
    let c = s.spawn("churn", Box::new(Churn { rounds: 3 }));
    s.join(b);
    s.join(c);

    let log = s.event_log().join("\n");
    assert!(log.contains("built depth=1"), "one persistent root: {log}");
    assert!(log.contains("verified phase=1"), "{log}");
    assert!(log.contains("verified phase=2"), "{log}");
    assert!(log.contains("builder done"), "{log}");

    let st = s.heap_stats();
    // 3 rounds x 40 garbage ints, reclaimed exactly; nothing else died.
    assert_eq!(st.reclaimed_objects, 120, "{log}");
    // Chain: 3 heads + nil + 3 pairs = 7 live objects.
    let rep = s.gc_collect_for_test();
    assert_eq!(rep.live_after, 7, "{log}");
    assert_eq!(rep.reclaimed_this_run, 0, "{log}");
}

// ------------------------------------------------------- 3. missed root dies

struct KeeperVsGarbage {
    phase: u8,
}

impl Step for KeeperVsGarbage {
    fn step(&mut self, cx: &mut Cx) -> Action {
        assert_eq!(self.phase, 0);
        self.phase = 1;
        let garbage = cx.alloc_int(1); // never rooted: protocol violation
        let keeper = cx.alloc_rooted_int(99);
        let rep = cx.gc_collect_for_test();
        assert!(cx.heap_get_int(garbage).is_none(), "unrooted object died");
        assert_eq!(cx.heap_get_int(keeper), Some(99), "rooted object lives");
        assert_eq!(rep.reclaimed_this_run, 1);
        assert_eq!(rep.live_after, 1);
        cx.log("missed-root behaved as specified");
        Action::Done
    }
}

#[test]
fn unrooted_value_is_reclaimed_rooted_value_survives() {
    let mut s = Scheduler::new();
    let h = s.spawn("t", Box::new(KeeperVsGarbage { phase: 0 }));
    s.join(h);
    assert!(s.event_log().iter().any(|l| l.contains("missed-root")));
}

// -------------------------------------- 4. unbuffered channel + in-flight GC

struct Sender {
    ch: spike_1_gc_scheduler::ChannelId,
    phase: u8,
}

struct Receiver {
    ch: spike_1_gc_scheduler::ChannelId,
    phase: u8,
}

impl Step for Sender {
    fn step(&mut self, cx: &mut Cx) -> Action {
        if self.phase == 1 {
            cx.log("sent-acked");
            return Action::Done;
        }
        self.phase = 1;
        let h = cx.alloc_rooted_int(7);
        let t = cx.alloc_rooted_int(8);
        let pair = cx.with_frame(|cx| cx.alloc_pair(h, t));
        cx.root(pair);
        cx.log("sending");
        Action::Send(self.ch, pair)
    }
}

impl Step for Receiver {
    fn step(&mut self, cx: &mut Cx) -> Action {
        if self.phase == 1 {
            return Action::Done;
        }
        match cx.take_inbox() {
            None => Action::Recv(self.ch),
            Some(v) => {
                self.phase = 1;
                cx.root(v);
                let (h, t) = cx.heap_get_pair(v).expect("pair survives");
                let (hv, tv) = (
                    cx.heap_get_int(h).expect("head survives"),
                    cx.heap_get_int(t).expect("tail survives"),
                );
                cx.log(format!("got {hv} {tv}"));
                Action::Yield
            }
        }
    }
}

#[test]
fn unbuffered_channel_moves_heap_values_through_forced_gc() {
    let mut s = Scheduler::new();
    let ch = s.channel();
    let tx = s.spawn("tx", Box::new(Sender { ch, phase: 0 }));
    let rx = s.spawn("rx", Box::new(Receiver { ch, phase: 0 }));

    // Sender runs first and parks with the pair in flight.
    assert!(s.drive_once());
    let rep = s.gc_collect_for_test();
    assert_eq!(rep.live_after, 3, "pair + 2 ints rooted via channel");
    assert_eq!(rep.reclaimed_this_run, 0);

    // Receiver rendezvous; value now sits in its inbox root.
    assert!(s.drive_once());
    let rep = s.gc_collect_for_test();
    assert_eq!(rep.live_after, 3, "pair + 2 ints rooted via inbox/shadow");
    assert_eq!(rep.reclaimed_this_run, 0);

    s.join(tx);
    s.join(rx);
    let log: Vec<&str> = s.event_log().iter().map(String::as_str).collect();
    assert_eq!(log, ["sending", "sent-acked", "got 7 8"]);
}

// ---------------------------------- 5. root-stack state across switches

struct Keeper {
    phase: u8,
    depth0: usize,
    val: Option<GcRef>,
}

impl Step for Keeper {
    fn step(&mut self, cx: &mut Cx) -> Action {
        match self.phase {
            0 => {
                let v = cx.alloc_rooted_int(4242);
                self.val = Some(v);
                self.depth0 = cx.shadow_len();
                cx.log(format!("keeper rooted depth={}", self.depth0));
                self.phase = 1;
                Action::Yield
            }
            1..=3 => {
                let v = self.val.expect("handle kept in task state");
                assert_eq!(
                    cx.shadow_len(),
                    self.depth0,
                    "shadow stack unchanged across switches"
                );
                assert!(cx.shadow_contains(v), "root still published");
                assert_eq!(cx.heap_get_int(v), Some(4242), "value intact");
                cx.log("keeper ok".to_owned());
                self.phase += 1;
                Action::Yield
            }
            _ => {
                cx.log("keeper done".to_owned());
                Action::Done
            }
        }
    }
}

#[test]
fn task_root_stack_stays_correct_across_context_switches() {
    let mut s = Scheduler::new();
    let k = s.spawn(
        "keeper",
        Box::new(Keeper {
            phase: 0,
            depth0: 0,
            val: None,
        }),
    );
    let c = s.spawn("churn", Box::new(Churn { rounds: 4 }));
    s.join(k);
    s.join(c);

    let oks = s.event_log().iter().filter(|l| *l == "keeper ok").count();
    assert_eq!(oks, 3, "keeper verified after every switch");
    assert_eq!(s.heap_stats().reclaimed_objects, 160, "4 rounds x 40");
    let rep = s.gc_collect_for_test();
    assert_eq!(rep.live_after, 1, "only the keeper int is live");
}

// ------------------------------------------- 6. allocation-threshold trigger

struct Hog {
    first: Option<GcRef>,
    steps: u32,
}

impl Step for Hog {
    fn step(&mut self, cx: &mut Cx) -> Action {
        if self.first.is_none() {
            self.first = Some(cx.alloc_rooted_int(777));
        }
        for i in 0..10 {
            cx.alloc_rooted_int(i);
        }
        let f = self.first.expect("first rooted");
        assert_eq!(cx.heap_get_int(f), Some(777), "root survives auto-GC");
        self.steps += 1;
        if self.steps >= 5 {
            cx.log("hog done".to_owned());
            return Action::Done;
        }
        Action::Yield
    }
}

#[test]
fn allocation_threshold_triggers_collection_automatically() {
    let mut s = Scheduler::new().with_threshold(256);
    let h = s.spawn(
        "hog",
        Box::new(Hog {
            first: None,
            steps: 0,
        }),
    );
    s.join(h);
    // `live_objects` is counted at the last collection; collect once
    // more so the count includes tail allocations after that run.
    s.gc_collect_for_test();
    let st = s.heap_stats();
    assert!(st.gc_runs >= 1, "threshold fired at least once: {st:?}");
    assert_eq!(st.live_objects, 51, "1 + 5x10 rooted ints: {st:?}");
    assert_eq!(st.reclaimed_objects, 0, "everything was rooted: {st:?}");
}

// ------------------------------------------------- 7. join drives, idempotent

#[test]
fn join_drives_other_tasks_and_is_idempotent() {
    let mut s = Scheduler::new();
    let a = s.spawn(
        "a",
        Box::new(Tick {
            name: "a",
            left: 0,
            steps: 0,
        }),
    );
    let b = s.spawn(
        "b",
        Box::new(Tick {
            name: "b",
            left: 2,
            steps: 0,
        }),
    );
    s.join(a);
    assert!(s.is_done(a));
    // Joining b also works after a; re-joining a is a no-op.
    s.join(b);
    s.join(a);
    assert!(s.is_done(b));
    let dones = s.event_log().iter().filter(|l| l.contains("done")).count();
    assert_eq!(dones, 2, "each task finished exactly once");
}
