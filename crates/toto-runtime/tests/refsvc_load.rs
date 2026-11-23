//! Wave 3 Stage 5 pool behavior under load + reference-service
//! load proof (real service, real auth, real database).
//!
//! The load program lives in the corpus project
//! (`tests/corpus/project/proj25_refsvc_data/load.gzv`) next to
//! the golden — one program, three harnesses: the corpus runs
//! `main.gzv`, this suite runs `load.gzv`, `bench.sh` times it.
//! Entry display path keeps `import "common.gzv"` resolving; the
//! reader serves stdlib from the embedded table and the project
//! file from disk. Everything the program needs arrives by
//! environment (no substitution, no secrets): DSN, port,
//! fixture dir, scales.
//!
//! What this proves (behavior classes, never timing): Phase A —
//! forced exhaustion under 8-way contention is the documented
//! `pool exhausted...` Err (never a stall); Phase C — the sized
//! pool serves every authed request (exact-shape match) with zero
//! other error classes, then recovers across `db_close_pool`.
//! Cancellation under load is a separate program below: the
//! waiter's scope fails with E0214 through the standard sweep
//! (no hang under contention), then the SAME process-global
//! pool serves a fresh burst (recycled, not leaked).
//!
//! SQLite always runs (temp file); Postgres/MySQL run when their
//! `TOTO_TEST_*_DSN` env is set (CI sets both — same pattern as
//! the `db_*` suites). Linux-only where noted (the client and
//! server are portable HTTP/TLS; the suites run where their
//! databases are reachable, i.e. the Linux check job).

use toto_compiler::project;
use toto_runtime::{RuntimeConfig, run_checked};

const LOAD_ENTRY: &str = "tests/corpus/project/proj25_refsvc_data/load.gzv";
const LOAD_SRC: &str = include_str!("../../../tests/corpus/project/proj25_refsvc_data/load.gzv");
const COMMON_SRC: &str =
    include_str!("../../../tests/corpus/project/proj25_refsvc_data/common.gzv");

fn case_dir() -> String {
    format!(
        "{}/../../tests/corpus/project/proj25_refsvc_data",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn db_path(test: &str) -> String {
    let p = std::env::temp_dir().join(format!(
        "toto-refsvc-load-{}-{}-{}",
        test,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    format!("sqlite:path={}", p.display())
}

static LOAD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_load(dsn: &str, port: u16) {
    // The program reads its DSN/port through process-global env
    // (same mechanism as `toto run` / bench.sh); Rust runs tests
    // in one process, so the load tests serialize here.
    let _guard = LOAD_LOCK.lock().expect("load lock");
    let fixdir = format!("{}/fixtures", case_dir());
    unsafe { std::env::set_var("TOTO_REFSVC_FIXDIR", &fixdir) };
    unsafe { std::env::set_var("TOTO_REFSVC_DSN", dsn) };
    unsafe { std::env::set_var("TOTO_REFSVC_PORT", port.to_string()) };
    // Smaller hammer for the gate (bench.sh measures the full 60-req load).
    unsafe { std::env::set_var("TOTO_REFSVC_REQS", "10") };
    let (project, checked) = project::load_and_check(
        LOAD_ENTRY,
        LOAD_SRC,
        &|p| {
            if p == "common.gzv" || p.ends_with("/common.gzv") {
                return Some(COMMON_SRC.to_string());
            }
            toto_compiler::stdlib::get(p).map(str::to_string)
        },
        &[],
    )
    .expect("load case checks");
    assert!(checked.uses_concurrency, "load case must park");
    let outcome = run_checked(
        &project.program,
        &checked,
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    );
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    let out = String::from_utf8_lossy(&outcome.stdout);
    assert!(out.contains("LOAD-OK"), "load gate marker missing:\n{out}");
    assert!(
        out.contains("phase-a ok=") && out.contains("recover=ok"),
        "load phases incomplete:\n{out}"
    );
    // Methodology evidence for the report (CI logs carry it via
    // --nocapture): the program's own LOAD line.
    for line in out.lines() {
        if line.starts_with("LOAD ") || line.starts_with("phase-") {
            println!("refsvc-load[{dsn}]: {line}");
        }
    }
}

#[test]
fn load_sqlite_pool_behavior() {
    run_load(&db_path("sqlite"), 48524);
}

#[test]
fn load_postgres_pool_behavior() {
    let dsn = std::env::var("TOTO_TEST_PG_DSN").unwrap_or_default();
    if dsn.is_empty() {
        println!("load_postgres_pool_behavior: SKIP (no TOTO_TEST_PG_DSN)");
        return;
    }
    run_load(&dsn, 48526);
}

#[test]
fn load_mysql_pool_behavior() {
    let dsn = std::env::var("TOTO_TEST_MYSQL_DSN").unwrap_or_default();
    if dsn.is_empty() {
        println!("load_mysql_pool_behavior: SKIP (no TOTO_TEST_MYSQL_DSN)");
        return;
    }
    run_load(&dsn, 48528);
}

// Cancellation under concurrent load: a slow waiter plus a
// fast burst share max 1; cancelling the waiter's scope fails
// the program with E0214 through the standard sweep (no hang,
// no worker consumed) — same mechanism as `db_pool`'s
// SRC_CANCEL, with load present.
const SRC_CANCEL_LOAD: &str = r#"import "gzv.std/db.gzv";
import "gzv.std/context.gzv";

fn burst(dsn: string, h: chan<int>) -> int {
    h <- 0;
    let mut bad: int = 0;
    let mut i: int = 0;
    while i < 20 {
        let r = db_query(dsn, "SELECT 1", []);
        let want: Result<Array<Array<DbVal>>, string> = Ok([[DbVal { kind: 1, i: 1, f: 0.0, s: "" }]]);
        if r != want {
            bad = bad + 1;
        } else {
        }
        i = i + 1;
    }
    if bad != 0 {
        return 1;
    }
    return 0;
}

fn waiter(dsn: string, h: chan<int>) -> int {
    let c = ctx_with_cancel();
    h <- c.handle;
    let got = db_query(dsn, "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 100000000) SELECT COUNT(*) FROM c", []);
    println(got);
    return 0;
}

fn t_cancel(handle: int) -> Result<bool, string> {
    ctx_cancel(handle)?;
    return Ok(true);
}

fn run() -> Result<string, string> {
    let dsn = "DBPATH";
    db_pool_config(dsn, 1, 5000)?;
    db_exec(dsn, "SELECT 1", [])?;
    let h = chan<int>(4);
    go {
        waiter(dsn, h);
    }
    go {
        burst(dsn, h);
    }
    go {
        burst(dsn, h);
    }
    let handle: int = <-h;
    let yes: Result<bool, string> = Ok(true);
    if t_cancel(handle) != yes {
        return Err("cancel call failed");
    } else {
    }
    return Ok("cancel sent");
}

fn main() -> int {
    println(run());
    return 0;
}
"#;

// Same pool, after the cancelled scope: the in-flight query
// finished on its thread and recycled to idle (documented
// guarantee) — a fresh max-1 burst serves clean, proving no
// leaked checkout wedges the pool. A leak would fail loudly
// here (`pool exhausted...`), never silently.
const SRC_RECOVER: &str = r#"import "gzv.std/db.gzv";

fn worker(dsn: string, out: chan<string>) -> int {
    let mut i: int = 0;
    while i < 10 {
        let r = db_query(dsn, "SELECT 1", []);
        let want: Result<Array<Array<DbVal>>, string> = Ok([[DbVal { kind: 1, i: 1, f: 0.0, s: "" }]]);
        if r != want {
            out <- "other";
            return 0;
        } else {
        }
        i = i + 1;
    }
    out <- "ok";
    return 0;
}

fn run() -> Result<string, string> {
    let dsn = "DBPATH";
    db_pool_config(dsn, 1, 2000)?;
    let out = chan<string>(2);
    go {
        worker(dsn, out);
    }
    go {
        worker(dsn, out);
    }
    let mut ok: int = 0;
    let mut k: int = 0;
    while k < 2 {
        let got = <-out;
        if got == "ok" {
            ok = ok + 1;
        } else {
        }
        k = k + 1;
    }
    if ok != 2 {
        return Err("RECOVER-FAIL");
    }
    return Ok("RECOVER-OK");
}

fn main() -> int {
    println(run());
    return 0;
}
"#;

fn run_src(src: &str) -> toto_runtime::RtOutcome {
    let (project, checked) = project::load_and_check(
        "main.gzv",
        src,
        &|p| toto_compiler::stdlib::get(p).map(str::to_string),
        &[],
    )
    .expect("cancel case checks");
    run_checked(
        &project.program,
        &checked,
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    )
}

#[test]
fn cancel_under_load_releases_and_recovers() {
    let dsn = db_path("cancel");
    let outcome = run_src(&SRC_CANCEL_LOAD.replace("DBPATH", &dsn));
    match &outcome.result {
        Ok(code) => panic!("cancelled db wait completed cleanly (exit {code})"),
        Err((diag, _trace)) => {
            assert_eq!(diag.code, "E0214", "cancel code");
            assert!(
                diag.message.contains("cancel"),
                "cancel message: {}",
                diag.message
            );
        }
    }
    let recover = run_src(&SRC_RECOVER.replace("DBPATH", &dsn));
    assert!(recover.result.is_ok(), "{:?}", recover.result);
    assert!(
        String::from_utf8_lossy(&recover.stdout).contains("RECOVER-OK"),
        "pool did not recover after cancel"
    );
}
