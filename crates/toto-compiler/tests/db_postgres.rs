//! Wave 3 Stage 1 Postgres interop (real server).
//!
//! DSN from `TOTO_TEST_PG_DSN` (default: the CI service —
//! `postgres:host=127.0.0.1 port=5432 dbname=totodb user=toto
//! password=toto-test-pw`). A driver without a passing test
//! against its real database does not ship: these tests fail
//! loudly (no skips, no doubles) when no server answers.
//! Linux-only (libpq has no macOS system counterpart).

#![cfg(target_os = "linux")]

use toto_compiler::dblib;

fn dsn() -> String {
    std::env::var("TOTO_TEST_PG_DSN").unwrap_or_else(|_| {
        "postgres:host=127.0.0.1 port=5432 dbname=totodb user=toto password=toto-test-pw"
            .to_string()
    })
}

fn table(name: &str) -> String {
    format!("w3_{name}_{}", std::process::id())
}

#[test]
fn crud_types_and_params() {
    let dsn = dsn();
    let t = table("crud");
    dblib::db_exec(&dsn, &format!("DROP TABLE IF EXISTS {t}"), &[]).expect("drop");
    dblib::db_exec(
        &dsn,
        &format!("CREATE TABLE {t} (a BIGINT, b DOUBLE PRECISION, c TEXT, d BOOLEAN, e NUMERIC, f TIMESTAMP)"),
        &[],
    )
    .expect("ddl");
    let p = [
        dblib::DbCell::int(-7),
        dblib::DbCell::float(2.5),
        dblib::DbCell::text("o'brien;x".to_string()),
        dblib::DbCell::bool_(true),
        dblib::DbCell::text("19.99".to_string()),
        dblib::DbCell::null(),
    ];
    // `?` placeholders translate to `$n` (server-side binding —
    // the quote in the text proves no interpolation).
    assert_eq!(
        dblib::db_exec(
            &dsn,
            &format!("INSERT INTO {t} VALUES (?, ?, ?, ?, ?, NULL)"),
            &p[..5]
        ),
        Ok(1)
    );
    let rows =
        dblib::db_query(&dsn, &format!("SELECT a, b, c, d, e, f FROM {t}"), &[]).expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0][0].kind, rows[0][0].i), (dblib::CELL_INT, -7));
    assert_eq!((rows[0][1].kind, rows[0][1].f), (dblib::CELL_FLOAT, 2.5));
    assert_eq!(
        (rows[0][2].kind, rows[0][2].s.as_str()),
        (dblib::CELL_TEXT, "o'brien;x")
    );
    assert_eq!((rows[0][3].kind, rows[0][3].i), (dblib::CELL_BOOL, 1));
    // NUMERIC arrives as text (precision kept — documented).
    assert_eq!(
        (rows[0][4].kind, rows[0][4].s.as_str()),
        (dblib::CELL_TEXT, "19.99")
    );
    assert_eq!(rows[0][5].kind, dblib::CELL_NULL);
    // Param count is positional: extra/missing placeholders fail.
    assert!(
        dblib::db_query(
            &dsn,
            &format!("SELECT a FROM {t} WHERE a = ? AND b = ?"),
            &[dblib::DbCell::int(1)]
        )
        .is_err()
    );
    dblib::db_exec(&dsn, &format!("DROP TABLE {t}"), &[]).expect("drop");
    dblib::db_close_pool(&dsn);
}

#[test]
fn update_delete_counts() {
    let dsn = dsn();
    let t = table("counts");
    dblib::db_exec(&dsn, &format!("DROP TABLE IF EXISTS {t}"), &[]).expect("drop");
    dblib::db_exec(&dsn, &format!("CREATE TABLE {t} (a BIGINT)"), &[]).expect("ddl");
    assert_eq!(
        dblib::db_exec(&dsn, &format!("INSERT INTO {t} VALUES (1)"), &[]),
        Ok(1)
    );
    assert_eq!(
        dblib::db_exec(&dsn, &format!("UPDATE {t} SET a = 9 WHERE a = 1"), &[]),
        Ok(1)
    );
    assert_eq!(
        dblib::db_exec(&dsn, &format!("UPDATE {t} SET a = 9 WHERE a = 42"), &[]),
        Ok(0)
    );
    assert_eq!(
        dblib::db_exec(&dsn, &format!("DELETE FROM {t}"), &[]),
        Ok(1)
    );
    dblib::db_exec(&dsn, &format!("DROP TABLE {t}"), &[]).expect("drop");
    dblib::db_close_pool(&dsn);
}

#[test]
fn errors_are_loud() {
    let dsn = dsn();
    let e = dblib::db_exec(&dsn, "INSERT INTO w3_nope_missing (a) VALUES (1)", &[])
        .expect_err("missing table");
    assert!(e.contains("does not exist"), "unexpected: {e}");
    let e = dblib::db_query(&dsn, "SELECT * FRM w3_nope", &[]).expect_err("bad sql");
    assert!(e.contains("syntax error"), "unexpected: {e}");
    // Unreachable server fails fast with the connect error, not a hang.
    let bad = "postgres:host=127.0.0.1 port=1 dbname=x user=x password=x connect_timeout=2";
    let e = dblib::db_exec(bad, "SELECT 1", &[]).expect_err("refused");
    assert!(!e.is_empty(), "connect error named");
    dblib::db_close_pool(bad);
    dblib::db_close_pool(&dsn);
}

#[test]
fn transactions_are_per_statement() {
    // Documented v1 limitation: no held-connection API, so a
    // multi-statement transaction cannot span checkouts. Each
    // statement runs in its own implicit transaction; a mid-batch
    // failure leaves earlier statements committed. The migrations
    // layer (Stage 2) designs around this with a ledger +
    // down-migrations, never assuming atomic batches.
    let dsn = dsn();
    let t = table("txn");
    dblib::db_exec(&dsn, &format!("DROP TABLE IF EXISTS {t}"), &[]).expect("drop");
    dblib::db_exec(&dsn, &format!("CREATE TABLE {t} (a BIGINT)"), &[]).expect("ddl");
    dblib::db_exec(&dsn, &format!("INSERT INTO {t} VALUES (1)"), &[]).expect("ins");
    // Second statement of the "batch" fails; the first stays.
    let r = dblib::db_exec(&dsn, "INSERT INTO w3_nope_missing (a) VALUES (1)", &[]);
    assert!(r.is_err());
    let rows = dblib::db_query(&dsn, &format!("SELECT COUNT(*) FROM {t}"), &[]).expect("count");
    assert_eq!(rows[0][0].i, 1, "first statement stayed committed");
    dblib::db_exec(&dsn, &format!("DROP TABLE {t}"), &[]).expect("drop");
    dblib::db_close_pool(&dsn);
}
