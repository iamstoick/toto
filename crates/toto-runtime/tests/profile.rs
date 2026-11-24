//! Wave 4 Stage 2 profiler proof: CPU sampling with green-thread
//! attribution plus D1 heap live tables, asserted against a real
//! (in-process) run. Deterministic by construction: CPU presence
//! (not exact counts — wall-clock sampling) plus exact heap live
//! counts (every object tagged; phased barriers make the last
//! sweep tally exact).
//!
//! The workload mirrors `tests/corpus/project/proj26_prof` (same
//! phased shape): two named workers fill arrays, hold them across
//! a barrier, main forces a final collection, all three sum. The
//! profile file comes from the exit dump (`TOTO_PROFILE=1`).

use toto_compiler::project;
use toto_runtime::{RuntimeConfig, run_checked};

const N: usize = 100000;
const ARR_BYTES: u64 = 32 + 16 * N as u64;

fn profile_src() -> String {
    format!(
        r#"fn worker_a(out: chan<int>, hold: chan<int>, n: int) -> int {{
    let a: Array<int> = [];
    let mut i: int = 0;
    while i < n {{
        push(a, i);
        i = i + 1;
    }}
    out <- len(a);
    let rel = <-hold;
    let mut s: int = 0;
    for x in a {{
        s = s + x;
    }}
    out <- s;
    return 0;
}}

fn worker_b(out: chan<int>, hold: chan<int>, n: int) -> int {{
    let b: Array<int> = [];
    let mut i: int = 0;
    while i < n {{
        push(b, i);
        i = i + 1;
    }}
    out <- len(b);
    let rel = <-hold;
    let mut s: int = 0;
    for x in b {{
        s = s + x;
    }}
    out <- s;
    return 0;
}}

fn run() -> Result<string, string> {{
    let n: int = {n};
    let ra = chan<int>(1);
    let ha = chan<int>(1);
    go {{
        worker_a(ra, ha, n);
    }}
    let lena = <-ra;
    let rb = chan<int>(1);
    let hb = chan<int>(1);
    go {{
        worker_b(rb, hb, n);
    }}
    let lenb = <-rb;
    let c: Array<int> = [];
    let mut i: int = 0;
    while i < n {{
        push(c, i);
        i = i + 1;
    }}
    ha <- 1;
    hb <- 1;
    let suma = <-ra;
    let sumb = <-rb;
    let mut sumc: int = 0;
    for x in c {{
        sumc = sumc + x;
    }}
    let want: int = n * (n - 1) / 2;
    if lena != n || lenb != n || len(c) != n {{
        return Err("length mismatch");
    }} else {{
    }}
    if suma != want || sumb != want || sumc != want {{
        return Err("sum mismatch");
    }} else {{
    }}
    return Ok("PROF-OK");
}}

fn main() -> int {{
    println(run());
    return 0;
}}
"#,
        n = N
    )
}

fn profile_path(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!(
        "toto-prof-{}-{}-{}.txt",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    p.display().to_string()
}

fn lines(path: &str) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("profile missing: {path}"));
    text.lines().map(str::to_string).collect()
}

fn samples_of(ls: &[String], task_prefix: &str) -> usize {
    let line = ls
        .iter()
        .find(|l| l.starts_with(task_prefix))
        .unwrap_or_else(|| panic!("missing {task_prefix} in {ls:?}"));
    line.rsplit('=')
        .next()
        .unwrap()
        .parse()
        .expect("sample count")
}

fn heap_of(ls: &[String], id: u32) -> (usize, u64) {
    let prefix = format!("heap {id} objects=");
    let line = ls
        .iter()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("missing {prefix} in {ls:?}"));
    let rest = &line[prefix.len()..];
    let mut parts = rest.split(" bytes=");
    let objs: usize = parts.next().unwrap().parse().expect("objs");
    let bytes: u64 = parts.next().unwrap().parse().expect("bytes");
    (objs, bytes)
}

#[test]
fn cpu_samples_attribute_every_task_and_heap_is_exact() {
    let out = profile_path("exact");
    // SAFETY: this is the only test in this binary, so no other
    // thread can observe these process-wide vars mid-mutation;
    // no other test binary reads them.
    unsafe {
        std::env::set_var("TOTO_PROFILE", "1");
        std::env::set_var("TOTO_PROFILE_OUT", &out);
        std::env::set_var("TOTO_PROFILE_HZ", "100");
    }
    let (project, checked) = project::load_and_check(
        "main.gzv",
        &profile_src(),
        &|p| toto_compiler::stdlib::get(p).map(str::to_string),
        &[],
    )
    .expect("profile program checks");
    assert!(checked.uses_concurrency, "profile case must park");
    let outcome = run_checked(
        &project.program,
        &checked,
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    );
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    let text = String::from_utf8_lossy(&outcome.stdout);
    assert!(text.contains("PROF-OK"), "workload ok: {text}");

    let ls = lines(&out);
    assert_eq!(ls.first().map(String::as_str), Some("profile v1"), "header");
    // Every worker sampled (presence, not exact counts:
    // wall-clock sampling is nondeterministic by design).
    // Task ids are spawn order on one worker: main 0, A 1, B 2.
    assert!(samples_of(&ls, "task 1 go samples=") > 0, "A sampled");
    assert!(samples_of(&ls, "task 2 go samples=") > 0, "B sampled");
    // Stacks name the worker functions (green-thread attribution:
    // a profile that cannot tell tasks apart is not sufficient).
    assert!(
        ls.iter().any(|l| l.starts_with("stack 1 go;worker_a ")),
        "worker_a stack: {ls:?}"
    );
    assert!(
        ls.iter().any(|l| l.starts_with("stack 2 go;worker_b ")),
        "worker_b stack: {ls:?}"
    );
    // Heap: each task holds its array plus its `for` snapshot
    // (iteration snapshots; frame-rooted locals stay rooted —
    // conservative roots, documented). Exact: 2 full arrays
    // per task; the per-task split is the attribution proof.
    for (id, want_objs, want_bytes) in [
        (0u32, 2usize, 2 * ARR_BYTES),
        (1u32, 2usize, 2 * ARR_BYTES),
        (2u32, 2usize, 2 * ARR_BYTES),
    ] {
        let (objs, bytes) = heap_of(&ls, id);
        assert_eq!(objs, want_objs, "task {id} objects");
        assert_eq!(bytes, want_bytes, "task {id} bytes");
    }
    assert!(
        ls.iter().any(|l| l == "kind Array objects=6"),
        "array total: {ls:?}"
    );
    let _ = std::fs::remove_file(&out);
}
