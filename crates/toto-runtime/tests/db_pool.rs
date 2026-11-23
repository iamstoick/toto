//! Wave 3 Stage 1 pool semantics under the runtime (blocking pool,
//! not workers): exhaustion, timeouts, cancellation, concurrent
//! correctness. Deterministic by construction (no timing
//! assertions — timeouts only bound how long a test may take, and
//! outcomes never depend on who wins a race).
//!
//! SQLite file DBs with unique paths (pools are process-global and
//! tests run in parallel — a shared DSN would collide). The
//! driver paths are proven in `db_sqlite.rs` / `db_postgres.rs` /
//! `db_mysql.rs`.

use toto_compiler::project;
use toto_runtime::{RuntimeConfig, run_checked};

fn db_path(test: &str) -> String {
    // Unique file DB per test invocation.
    let p = std::env::temp_dir().join(format!(
        "toto-db-pool-{}-{}-{}",
        test,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    format!("sqlite:path={}", p.display())
}

fn run_src(src: &str) -> toto_runtime::RtOutcome {
    let (project, checked) = project::load_and_check(
        "main.gzv",
        src,
        &|p| toto_compiler::stdlib::get(p).map(str::to_string),
        &[],
    )
    .expect("db pool program checks");
    assert!(checked.uses_concurrency, "db pool case must park");
    run_checked(
        &project.program,
        &checked,
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    )
}

const SRC_EXHAUST: &str = r#"import "gzv.std/db.gzv";

fn slow(dsn: string, out: chan<Result<Array<Array<DbVal>>, string>>) -> int {
    out <- db_query(dsn, "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 20000000) SELECT COUNT(*) FROM c", []);
    return 0;
}

fn run() -> Result<string, string> {
    let dsn = "DBPATH";
    db_exec(dsn, "SELECT 1", [])?;
    db_pool_config(dsn, 1, 200)?;
    let out = chan<Result<Array<Array<DbVal>>, string>>(2);
    go {
        slow(dsn, out);
    }
    go {
        slow(dsn, out);
    }
    let cell = DbVal { kind: 1, i: 20000000, f: 0.0, s: "" };
    let want_ok: Result<Array<Array<DbVal>>, string> = Ok([[cell]]);
    let want_err: Result<Array<Array<DbVal>>, string> = Err("pool exhausted (1 connections in use, max 1)");
    let mut exhausted: int = 0;
    let mut ok: int = 0;
    let mut k: int = 0;
    while k < 2 {
        let got = <-out;
        if got == want_err {
            exhausted = exhausted + 1;
        } else {
        }
        if got == want_ok {
            ok = ok + 1;
        } else {
        }
        k = k + 1;
    }
    db_close_pool(dsn)?;
    if exhausted == 1 && ok == 1 {
        return Ok("ok");
    }
    return Err("exhausted=" + to_string(exhausted) + " ok=" + to_string(ok));
}

fn main() -> int {
    let want: Result<string, string> = Ok("ok");
    let r = run();
    if r == want {
        return 0;
    } else {
        println(r);
        return 1;
    }
}
"#;

const SRC_CANCEL: &str = r#"import "gzv.std/db.gzv";
import "gzv.std/context.gzv";

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

fn main() -> int {
    let yes: Result<bool, string> = Ok(true);
    let dsn = "DBPATH";
    let h = chan<int>(1);
    go {
        waiter(dsn, h);
    }
    let handle: int = <-h;
    if t_cancel(handle) != yes {
        println("FAIL cancel");
        return 1;
    } else {
    }
    return 0;
}
"#;

const SRC_HAMMER: &str = r#"import "gzv.std/db.gzv";

fn worker(dsn: string, base: int, out: chan<Result<int, string>>) -> int {
    let want_one: Result<int, string> = Ok(1);
    let mut i: int = 0;
    while i < 25 {
        let r = db_exec(dsn, "INSERT INTO hammer (v) VALUES (?)", [db_int(base + i)]);
        if r != want_one {
            out <- r;
            return 0;
        } else {
        }
        i = i + 1;
    }
    out <- Ok(25);
    return 0;
}

fn run() -> Result<string, string> {
    let dsn = "DBPATH";
    db_exec(dsn, "CREATE TABLE hammer (v INTEGER)", [])?;
    db_pool_config(dsn, 4, 5000)?;
    let done = chan<Result<int, string>>(4);
    go {
        worker(dsn, 0, done);
    }
    go {
        worker(dsn, 1000, done);
    }
    go {
        worker(dsn, 2000, done);
    }
    go {
        worker(dsn, 3000, done);
    }
    let mut ok: int = 0;
    let mut k: int = 0;
    while k < 4 {
        let got = <-done;
        let want: Result<int, string> = Ok(25);
        if got == want {
            ok = ok + 1;
        } else {
        }
        k = k + 1;
    }
    let rows = db_query(dsn, "SELECT COUNT(*), SUM(v) FROM hammer", [])?;
    let total: int = rows[0][0].i;
    let sum: int = rows[0][1].i;
    db_close_pool(dsn)?;
    if ok == 4 && total == 100 && sum == 151200 {
        return Ok("ok");
    }
    return Err("hammer mismatch");
}

fn main() -> int {
    let want: Result<string, string> = Ok("ok");
    let r = run();
    if r == want {
        return 0;
    } else {
        println(r);
        return 1;
    }
}
"#;

/// Pool exhaustion is a loud `Err`, never a silent stall: with
/// max=1, exactly one of two overlapping holders fails with the
/// documented message while the other succeeds. Who wins is
/// scheduling-dependent, so the program aggregates
/// order-independently ("exhausted=1 ok=1" either way) — the
/// golden is deterministic.
#[test]
fn exhaustion_is_loud_and_counted() {
    let outcome = run_src(&SRC_EXHAUST.replace("DBPATH", &db_path("exhaust")));
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    assert_eq!(outcome.stdout, b"", "no output on the deterministic path");
}

/// Cancelling a scope releases a task parked on a DB wait with
/// E0214 through the standard sweep (same as TLS waits); the
/// in-flight pool job finishes on its thread and its completion
/// is dropped by the stale guard — no hang, no leak. The child
/// snapshots its own scope and parks AFTER handing main the
/// handle; whether the cancel lands before or during the park,
/// the outcome is E0214 either way (fail-fast at entry or
/// release at drain) — deterministic.
#[test]
fn cancel_releases_parked_db_wait() {
    let outcome = run_src(&SRC_CANCEL.replace("DBPATH", &db_path("cancel")));
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
}

/// Concurrent hammer through one pool: N tasks x M inserts each
/// (pool max=4, parallel checkouts), then an exact count + sum.
/// Proves one-task-at-a-time handles under load with no lost
/// writes and no cross-talk between connections.
#[test]
fn concurrent_hammer_keeps_every_row() {
    let outcome = run_src(&SRC_HAMMER.replace("DBPATH", &db_path("hammer")));
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    assert_eq!(outcome.stdout, b"", "no output on the deterministic path");
}
