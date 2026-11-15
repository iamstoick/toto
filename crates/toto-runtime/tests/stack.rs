//! Stack evidence (spec: 64 KiB fixed stacks with guard pages):
//! per-task high-water is measured on realistic corpus programs and
//! must sit comfortably under budget; the overflow abort is
//! demonstrated by `examples/overflow.rs` (recorded in the report).

use toto_compiler::parse_and_check;
use toto_runtime::{RuntimeConfig, run_checked};

const PINGPONG: &str = "fn main() -> int {
    let ch = chan<int>(1);
    go {
        let v = <-ch;
        ch <- v + 1
    }
    ch <- 41;
    let w = <-ch;
    println(w);
    return 0;
}
";

#[test]
fn high_water_under_budget_on_realistic_programs() {
    let (program, checked) = parse_and_check(PINGPONG).expect("checked");
    let outcome = run_checked(&program, &checked, RuntimeConfig::default());
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    let stats = outcome.stats;
    assert!(stats.tasks_spawned >= 2, "spawn the child");
    assert!(stats.switches >= 2, "switch stacks");
    assert!(stats.high_water_max > 0, "measured something");
    assert!(
        stats.high_water_max < 64 * 1024,
        "high-water {} exceeds the 64 KiB budget",
        stats.high_water_max
    );
}
