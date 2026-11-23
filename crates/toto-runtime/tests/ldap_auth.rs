//! Wave 3 Stage 3 LDAP interop (real OpenLDAP).
//!
//! DSN parts come from the environment (defaults: the CI
//! service — `127.0.0.1:389`, `dc=toto,dc=test`, admin +
//! alice/bob from the provisioned seed). Nothing secret is
//! committed: the harness reads env, the Toto programs take
//! their credentials by string substitution (`DBPATH`-style
//! `%%LDAP_*%%` placeholders), and fixtures carry placeholders
//! only. A driver without a passing test against its real
//! database does not ship: these tests fail loudly (no skips,
//! no doubles) when no server answers.
//! Linux-only (libldap has no macOS system counterpart).

#![cfg(target_os = "linux")]

use toto_compiler::project;
use toto_runtime::{RuntimeConfig, run_checked};

fn env(name: &str, dflt: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| dflt.to_string())
}

fn host() -> String {
    env("TOTO_TEST_LDAP_HOST", "127.0.0.1")
}
fn port() -> String {
    env("TOTO_TEST_LDAP_PORT", "389")
}
fn admin_dn() -> String {
    env("TOTO_TEST_LDAP_ADMIN_DN", "cn=admin,dc=toto,dc=test")
}
fn admin_pw() -> String {
    env("TOTO_TEST_LDAP_ADMIN_PW", "toto-admin-pw")
}
fn user_pw() -> String {
    env("TOTO_TEST_LDAP_USER_PW", "alice-pw")
}
fn base() -> String {
    env("TOTO_TEST_LDAP_BASE", "ou=users,dc=toto,dc=test")
}

fn run_src(src: &str) -> toto_runtime::RtOutcome {
    let (project, checked) = project::load_and_check(
        "main.gzv",
        src,
        &|p| toto_compiler::stdlib::get(p).map(str::to_string),
        &[],
    )
    .expect("ldap case checks");
    run_checked(
        &project.program,
        &checked,
        RuntimeConfig {
            workers: 1,
            ..RuntimeConfig::default()
        },
    )
}

const PROG: &str = r#"
import "gzv.std/ldap.gzv";
import "gzv.std/json.gzv";

fn check(cond: bool, msg: string) -> Result<bool, string> {
    if cond {
        return Ok(true);
    } else {
        return Err(msg);
    }
}

fn want_bad() -> Result<int, string> {
    return Err("ldap bind: Invalid credentials (code 49)");
}

fn run() -> Result<string, string> {
    // Bind paths (interpreter-shaped calls run through the
    // runtime pool here — same blocking policy, both engines).
    check(ldap_bind("%%HOST%%", %%PORT%%, "uid=alice,ou=users,dc=toto,dc=test", "%%ALICE_PW%%")? == 0, "alice binds")?;
    let bad = ldap_bind("%%HOST%%", %%PORT%%, "uid=alice,ou=users,dc=toto,dc=test", "wrong");
    check(bad == want_bad(), "wrong password loud")?;
    // Search with an allowlist (admin binds for the subtree).
    let js = ldap_search("%%HOST%%", %%PORT%%, "%%ADMIN_DN%%", "%%ADMIN_PW%%", "%%BASE%%", "(uid=alice)", "cn,mail")?;
    let v = json_parse(js)?;
    check(json_array_len(v)? == 1, "one entry")?;
    let e0 = json_array_get(v, 0)?;
    check(json_as_string(json_object_get(e0, "dn")?)? == "uid=alice,ou=users,dc=toto,dc=test", "dn")?;
    let cn = json_array_get(json_object_get(json_object_get(e0, "attrs")?, "cn")?, 0)?;
    check(json_as_string(cn)? == "Alice Liddell", "cn value")?;
    // Bad base is loud with the LDAP code, never silent.
    let r = ldap_search("%%HOST%%", %%PORT%%, "%%ADMIN_DN%%", "%%ADMIN_PW%%", "ou=nope,dc=toto,dc=test", "(uid=alice)", "cn");
    let want: Result<string, string> = Err("ldap search: No such object (code 32)");
    check(r == want, "bad base loud")?;
    return Ok("ldap-ok");
}

fn main() -> int {
    let want: Result<string, string> = Ok("ldap-ok");
    let r = run();
    if r == want {
        return 0;
    } else {
        println(r);
        return 1;
    }
}
"#;

#[test]
fn ldap_bind_search_runtime() {
    let src = PROG
        .replace("%%HOST%%", &host())
        .replace("%%PORT%%", &port())
        .replace("%%ADMIN_DN%%", &admin_dn())
        .replace("%%ADMIN_PW%%", &admin_pw())
        .replace("%%ALICE_PW%%", &user_pw())
        .replace("%%BASE%%", &base());
    let outcome = run_src(&src);
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    assert_eq!(outcome.stdout, b"", "no output on the green path");
}

#[test]
fn ldap_unreachable_fails_fast() {
    // Wrong port on loopback refuses immediately (no hang, no
    // pool stall) — the native call, no Toto involved.
    let e = toto_compiler::ldaplib::search(
        "127.0.0.1",
        1,
        "cn=admin,dc=toto,dc=test",
        "x",
        "dc=toto,dc=test",
        "(uid=alice)",
        "cn",
    )
    .expect_err("refused");
    assert!(!e.is_empty(), "connect error named");
}
