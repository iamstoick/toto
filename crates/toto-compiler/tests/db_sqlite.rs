//! Wave 3 Stage 1 SQLite + framing unit tests (no server: `:memory:`
//! and tempfiles). Driver interop against real Postgres/MySQL
//! lives in `db_postgres.rs` / `db_mysql.rs` (Linux CI services);
//! pool semantics under the runtime live in
//! `toto-runtime/tests/db_pool.rs`.

use toto_compiler::dblib;

/// Unique file DSN per test (pools are process-global and tests
/// run in parallel — a shared `:memory:` DSN would collide).
fn tmpdsn(name: &str) -> (String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "toto-db-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let path = dir.join("t.db");
    (format!("sqlite:path={}", path.display()), dir)
}

#[test]
fn crud_types_and_params() {
    let (dsn, dir) = tmpdsn("crud");
    assert_eq!(
        dblib::db_exec(
            &dsn,
            "CREATE TABLE t (a INTEGER, b REAL, c TEXT, d INTEGER, e TEXT)",
            &[]
        ),
        Ok(0)
    );
    // Multibyte text, quotes, separators — the `?` walk and the
    // char-count framing must round-trip all of it.
    let p = vec![
        dblib::DbCell::int(-7),
        dblib::DbCell::float(2.5),
        dblib::DbCell::text("héllo;?\"'".to_string()),
        dblib::DbCell::bool_(true),
        dblib::DbCell::null(),
    ];
    assert_eq!(
        dblib::db_exec(&dsn, "INSERT INTO t VALUES (?, ?, ?, ?, ?)", &p),
        Ok(1)
    );
    let rows = dblib::db_query(&dsn, "SELECT a, b, c, d, e FROM t", &[]).expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].kind, toto_compiler::dblib::CELL_INT);
    assert_eq!(rows[0][0].i, -7);
    assert_eq!(rows[0][1].kind, toto_compiler::dblib::CELL_FLOAT);
    assert_eq!(rows[0][1].f, 2.5);
    assert_eq!(rows[0][2].s, "héllo;?\"'");
    assert_eq!(rows[0][3].kind, toto_compiler::dblib::CELL_INT);
    assert_eq!(rows[0][3].i, 1);
    assert_eq!(rows[0][4].kind, toto_compiler::dblib::CELL_NULL);
    // Empty result set is Ok(empty), not an error.
    let empty = dblib::db_query(
        &dsn,
        "SELECT a FROM t WHERE a = ?",
        &[dblib::DbCell::int(999)],
    )
    .expect("empty");
    assert!(empty.is_empty());
    dblib::db_close_pool(&dsn);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn update_delete_counts() {
    let (dsn, dir) = tmpdsn("counts");
    dblib::db_exec(&dsn, "CREATE TABLE u (a INTEGER)", &[]).expect("ddl");
    assert_eq!(dblib::db_exec(&dsn, "INSERT INTO u VALUES (1)", &[]), Ok(1));
    assert_eq!(dblib::db_exec(&dsn, "INSERT INTO u VALUES (2)", &[]), Ok(1));
    // DDL affected counts are driver-defined (SQLite reports its
    // internal counter — 1 here; pg/mysql report 0): assert
    // success, not the count. DML counts are exact.
    dblib::db_exec(&dsn, "CREATE INDEX iu ON u (a)", &[]).expect("ddl index");
    assert_eq!(
        dblib::db_exec(&dsn, "UPDATE u SET a = 9 WHERE a = 1", &[]),
        Ok(1)
    );
    assert_eq!(
        dblib::db_exec(&dsn, "UPDATE u SET a = 9 WHERE a = 42", &[]),
        Ok(0)
    );
    assert_eq!(dblib::db_exec(&dsn, "DELETE FROM u", &[]), Ok(2));
    dblib::db_close_pool(&dsn);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn errors_are_loud() {
    let (dsn, dir) = tmpdsn("errors");
    let e =
        dblib::db_exec(&dsn, "INSERT INTO nope (a) VALUES (1)", &[]).expect_err("missing table");
    assert!(e.contains("no such table"), "unexpected: {e}");
    let e = dblib::db_query(&dsn, "SELECT * FRM t", &[]).expect_err("bad sql");
    assert!(e.contains("syntax error"), "unexpected: {e}");
    let e = dblib::db_exec("nosuchdb:x=1", "SELECT 1", &[]).expect_err("bad driver");
    assert!(e.contains("unknown database driver"), "unexpected: {e}");
    let e = dblib::db_exec("sqlite:", "SELECT 1", &[]).expect_err("empty dsn");
    assert!(!e.is_empty());
    dblib::db_close_pool(&dsn);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn framing_roundtrip() {
    let cells = vec![
        dblib::DbCell::null(),
        dblib::DbCell::int(-42),
        dblib::DbCell::float(0.5),
        dblib::DbCell::text("a;b:?héllo".to_string()),
        dblib::DbCell::bool_(true),
    ];
    let s = dblib::encode_params(&cells);
    assert_eq!(s, "P5:0,0:;1,3:-42;2,3:0.5;3,10:a;b:?héllo;4,1:1;");
    let back = dblib::decode_params(&s).expect("decode");
    assert_eq!(back.len(), 5);
    assert_eq!(back[2].f, 0.5);
    assert_eq!(back[3].s, "a;b:?héllo");
    let rows = dblib::encode_rows(&[cells]);
    assert!(String::from_utf8(rows).is_ok(), "rows framing is UTF-8");
    // Malformed input is Err, never silent/panicking.
    assert!(dblib::decode_params("P1:1,2:-7").is_err());
    assert!(dblib::decode_params("P9:").is_err());
    assert!(dblib::decode_params("bogus").is_err());
}

#[test]
fn placeholder_translation_skips_literals() {
    // `?` inside quotes/comments is not a placeholder.
    let sql = "SELECT * FROM t WHERE a = ? AND b = 'it''s ?' -- trailing ?\n AND c = /* ? */ ?";
    let out = dblib::translate_pg(sql);
    assert_eq!(
        out,
        "SELECT * FROM t WHERE a = $1 AND b = 'it''s ?' -- trailing ?\n AND c = /* ? */ $2"
    );
    let my = dblib::split_placeholders(sql);
    assert_eq!(my.len(), 3, "two placeholders: {my:?}");
    // Mismatched counts are loud at build time (MySQL path).
    assert_eq!(my[0], "SELECT * FROM t WHERE a = ");
    assert!(!my[2].ends_with(" ?"));
}

#[test]
fn pool_config_and_close() {
    let (dsn, dir) = tmpdsn("poolcfg");
    dblib::db_pool_config(&dsn, 3, 100).expect("config");
    assert!(
        dblib::db_pool_config(&dsn, 0, 100).is_err(),
        "max 0 rejected"
    );
    dblib::db_close_pool(&dsn);
    // Closing twice is fine (idempotent, test isolation).
    dblib::db_close_pool(&dsn);
    // Pool works after re-configure (fresh generation).
    dblib::db_pool_config(&dsn, 2, 5000).expect("reconfig");
    assert_eq!(dblib::db_exec(&dsn, "SELECT 1", &[]), Ok(0));
    dblib::db_close_pool(&dsn);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn file_db_roundtrip() {
    let (dsn, dir) = tmpdsn("file");
    dblib::db_exec(&dsn, "CREATE TABLE k (v TEXT)", &[]).expect("ddl");
    dblib::db_exec(
        &dsn,
        "INSERT INTO k VALUES (?)",
        &[dblib::DbCell::text("x".to_string())],
    )
    .expect("ins");
    let rows = dblib::db_query(&dsn, "SELECT v FROM k", &[]).expect("sel");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].s, "x");
    dblib::db_close_pool(&dsn);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
