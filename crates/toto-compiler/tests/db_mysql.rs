//! Wave 3 Stage 1 MySQL interop (real server).
//!
//! DSN from `TOTO_TEST_MYSQL_DSN` (default: the CI service —
//! `mysql:host=127.0.0.1 port=3306 dbname=totodb user=toto
//! password=toto-test-pw`). A driver without a passing test
//! against its real database does not ship: these tests fail
//! loudly (no skips, no doubles) when no server answers.
//! Linux-only (libmysqlclient has no macOS system counterpart).

#![cfg(target_os = "linux")]

use toto_compiler::dblib;

fn dsn() -> String {
    std::env::var("TOTO_TEST_MYSQL_DSN").unwrap_or_else(|_| {
        "mysql:host=127.0.0.1 port=3306 dbname=totodb user=toto password=toto-test-pw".to_string()
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
        &format!(
            "CREATE TABLE {t} (a BIGINT, b DOUBLE, c TEXT, d TINYINT, e DECIMAL(10,2), f DATETIME)"
        ),
        &[],
    )
    .expect("ddl");
    let p = [
        dblib::DbCell::int(-7),
        dblib::DbCell::float(2.5),
        dblib::DbCell::text("o'brien;x\\".to_string()),
        dblib::DbCell::bool_(true),
        dblib::DbCell::text("19.99".to_string()),
        dblib::DbCell::null(),
    ];
    // `?` params escape through the library's own function —
    // the quote/backslash prove no interpolation.
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
        (dblib::CELL_TEXT, "o'brien;x\\")
    );
    // MySQL has no bool: TINYINT reads as int (documented).
    assert_eq!((rows[0][3].kind, rows[0][3].i), (dblib::CELL_INT, 1));
    // DECIMAL "19.99" is float-shaped, so it decodes as float
    // (documented: for exact decimals, CAST to CHAR in SQL or
    // store cents as integers).
    assert_eq!((rows[0][4].kind, rows[0][4].f), (dblib::CELL_FLOAT, 19.99));
    assert_eq!(rows[0][5].kind, dblib::CELL_NULL);
    // Placeholder/arg count mismatch is loud at build time.
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
fn shape_decode_rule() {
    // Documented wart: the text protocol carries no types on this
    // path, so values decode by shape. A TEXT column holding
    // "123" reads as int; compare loosely or CAST in SQL when
    // the distinction matters.
    let dsn = dsn();
    let t = table("shape");
    dblib::db_exec(&dsn, &format!("DROP TABLE IF EXISTS {t}"), &[]).expect("drop");
    dblib::db_exec(&dsn, &format!("CREATE TABLE {t} (c TEXT)"), &[]).expect("ddl");
    for v in ["123", "1.5", "abc", "", "-42", "1e3", " 12", "12 "] {
        dblib::db_exec(
            &dsn,
            &format!("INSERT INTO {t} VALUES (?)"),
            &[dblib::DbCell::text(v.to_string())],
        )
        .expect("ins");
    }
    // No ORDER BY collation assumptions (PAD SPACE and friends):
    // collect by kind and match values instead.
    let rows = dblib::db_query(&dsn, &format!("SELECT c FROM {t}"), &[]).expect("query");
    let mut ints = Vec::new();
    let mut floats = Vec::new();
    let mut texts = Vec::new();
    for r in &rows {
        match r[0].kind {
            k if k == dblib::CELL_INT => ints.push(r[0].i),
            k if k == dblib::CELL_FLOAT => floats.push(r[0].f),
            _ => texts.push(r[0].s.clone()),
        }
    }
    ints.sort();
    assert_eq!(ints, vec![-42, 123]);
    assert_eq!(floats.len(), 2);
    assert!(floats.contains(&1.5) && floats.contains(&1000.0));
    texts.sort();
    assert_eq!(texts, vec!["", " 12", "12 ", "abc"]);
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
    assert!(e.contains("w3_nope_missing"), "unexpected: {e}");
    let e = dblib::db_query(&dsn, "SELECT * FRM w3_nope", &[]).expect_err("bad sql");
    assert!(e.contains("syntax"), "unexpected: {e}");
    // Unreachable server fails fast with the connect error, not a hang.
    let bad = "mysql:host=127.0.0.1 port=1 dbname=x user=x password=x connect_timeout=2";
    let e = dblib::db_exec(bad, "SELECT 1", &[]).expect_err("refused");
    assert!(!e.is_empty(), "connect error named");
    dblib::db_close_pool(bad);
    dblib::db_close_pool(&dsn);
}
