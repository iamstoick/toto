//! Wave 3 Stage 3 OIDC interop (real Keycloak).
//!
//! Everything is env-driven (defaults: the CI service):
//! `TOTO_TEST_OIDC_BASE` (e.g. `http://127.0.0.1:8080`),
//! `TOTO_TEST_OIDC_REALM` (default `toto`),
//! `TOTO_TEST_OIDC_CLIENT_{ID,SECRET,REDIRECT}` (default
//! `refsvc-rs256` + the provisioned secret + loopback
//! redirect), `TOTO_TEST_OIDC_USER` / `TOTO_TEST_OIDC_PASSWORD`
//! (default `alice` / per-env). The Toto program below takes
//! its parameters by string substitution (`%%...%%`
//! placeholders) — nothing secret is committed; the CI
//! workflow provisions the realm/client/user before running.
//! Linux-only where noted (the client is portable HTTP; the
//! suite runs wherever Keycloak is reachable, i.e. the Linux
//! check job via services).
//!
//! What this proves end to end: discovery, auth-URL
//! construction (+PKCE), password grant, JWKS fetch,
//! RS256 ID-token verification (iss/aud/expiry), userinfo.
//! The §6 browser-leg SSO demonstration drives the same
//! endpoints with form login (separate run, same provider).

use toto_compiler::project;
use toto_runtime::{RuntimeConfig, run_checked};

fn env(name: &str, dflt: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| dflt.to_string())
}

fn run_src(src: &str) -> toto_runtime::RtOutcome {
    let (project, checked) = project::load_and_check(
        "main.gzv",
        src,
        &|p| toto_compiler::stdlib::get(p).map(str::to_string),
        &[],
    )
    .expect("oidc case checks");
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
import "gzv.std/oidc.gzv";
import "gzv.std/json.gzv";
import "gzv.std/jwt.gzv";
import "gzv.std/strings.gzv";

fn run() -> Result<string, string> {
    let base = "%%BASE%%";
    let realm = "%%REALM%%";
    let client = "%%CLIENT%%";
    let secret = "%%SECRET%%";
    let user = "%%USER%%";
    let password = "%%PASSWORD%%";
    let redirect = "%%REDIRECT%%";
    // Discovery + auth-URL construction (PKCE included).
    let cfg = oidc_discover(base + "/realms/" + realm, "")?;
    let pkce = oidc_pkce()?;
    let auth_url = oidc_auth_url(cfg, client, redirect, "openid", "state-1", "nonce-1", pkce[1])?;
    if !starts_with(auth_url, cfg.auth_endpoint) {
        return Err("auth url not on the auth endpoint");
    }
    if !starts_with(auth_url, cfg.auth_endpoint + "?response_type=code&") {
        return Err("auth url shape wrong");
    }
    // Password grant (automation seam) + JWKS + verify.
    let toks = oidc_password_grant(cfg, client, secret, user, password)?;
    let jwks = oidc_fetch_jwks(cfg, "")?;
    // Read `exp` unverified, then verify inside validity.
    let raw_parts = split(toks.id_token, ".");
    let raw_payload = jwt_ascii_string(jwt_b64_decode(raw_parts[1])?)?;
    let exp = json_as_int(json_object_get(json_parse(raw_payload)?, "exp")?)?;
    let iss = base + "/realms/" + realm;
    let payload = oidc_verify_id_token(jwks, toks.id_token, exp - 60, 60, iss, client)?;
    // Userinfo over Bearer carries the same subject.
    let me = oidc_userinfo(cfg, toks.access_token, "")?;
    let who = json_parse(me)?;
    let sub = json_as_string(json_object_get(who, "sub")?)?;
    let psub = json_as_string(json_object_get(json_parse(payload)?, "sub")?)?;
    if sub != psub {
        return Err("userinfo sub mismatch");
    }
    return Ok("oidc-ok");
}

fn main() -> int {
    let want: Result<string, string> = Ok("oidc-ok");
    let r = run();
    if r == want {
        return 0;
    } else {
        println(r);
        return 1;
    }
}
"#;

/// §6 browser-leg SSO demonstration (same provider, separate run):
/// the full authorization-code leg with form login, driven
/// entirely through the Wave-2 HTTP client — no browser, no
/// curl. Discovery → auth URL → login form (200, or one manual
/// 302 hop) → credential POST with a manual cookie jar →
/// `code` extraction → code exchange → RS256 JWKS verify →
/// userinfo with sub match. Prints fixed step lines only (no
/// tokens, no codes, no secrets — all dynamic values stay
/// in-memory); any deviation fails loudly with its step.
const PROG_SSO: &str = r#"import "gzv.std/oidc.gzv";
import "gzv.std/json.gzv";
import "gzv.std/jwt.gzv";
import "gzv.std/strings.gzv";
import "gzv.std/http.gzv";

fn qparam(url: string, name: string) -> Result<string, string> {
    let key = name + "=";
    let i = index_of(url, key);
    if i < 0 {
        return Err("missing param " + name);
    }
    let rest = substring(url, i + len(key), len(url));
    let amp = index_of(rest, "&");
    if amp < 0 {
        return Ok(rest);
    }
    return Ok(substring(rest, 0, amp));
}

fn unescape_amp(s: string) -> string {
    let parts = split(s, "&amp;");
    let mut out = parts[0];
    let mut i = 1;
    while i < len(parts) {
        out = out + "&" + parts[i];
        i = i + 1;
    }
    return out;
}

// First form action at/after the first <form tag, entities
// unescaped (Keycloak renders &amp; in the query).
fn form_action(html: string) -> Result<string, string> {
    let fi = index_of(html, "<form");
    if fi < 0 {
        return Err("no form tag");
    }
    let fromform = substring(html, fi, len(html));
    let key = "action=\"";
    let i = index_of(fromform, key);
    if i < 0 {
        return Err("no form action");
    }
    let rest = substring(fromform, i + len(key), len(fromform));
    let q = index_of(rest, "\"");
    if q < 0 {
        return Err("unterminated action");
    }
    return Ok(unescape_amp(substring(rest, 0, q)));
}

fn jar_add(jar: string, hs: Array<HttpHeader>) -> string {
    let mut j = jar;
    for h in hs {
        if lower(h.name) == "set-cookie" {
            let mut pair = h.value;
            let semi = index_of(h.value, ";");
            if semi >= 0 {
                pair = substring(h.value, 0, semi);
            } else {
            }
            if j == "" {
                j = pair;
            } else {
                j = j + "; " + pair;
            }
        } else {
        }
    }
    return j;
}

fn jar_headers(jar: string) -> Array<HttpHeader> {
    if jar == "" {
        let e: Array<HttpHeader> = [];
        return e;
    } else {
    }
    return [HttpHeader { name: "Cookie", value: jar }];
}

fn run() -> Result<string, string> {
    let base = "%%BASE%%";
    let realm = "%%REALM%%";
    let client = "%%CLIENT%%";
    let secret = "%%SECRET%%";
    let user = "%%USER%%";
    let password = "%%PASSWORD%%";
    let redirect = "%%REDIRECT%%";
    let opts = HttpClientOpts { max_body: 1048576, redirects: 0, host_allow: [] };
    let b: Array<int> = [];
    let cfg = oidc_discover(base + "/realms/" + realm, "")?;
    println("sso-discover=ok");
    let pkce = oidc_pkce()?;
    let auth_url = oidc_auth_url(cfg, client, redirect, "openid", "state-1", "nonce-1", pkce[1])?;
    // Browser leg, driven manually: the auth endpoint serves
    // the login form (200) or redirects to it once (302).
    let e0: Array<HttpHeader> = [];
    let r1 = http_do(HttpClientReq { method: "GET", url: auth_url, headers: e0, body: b, ca_pem: "" }, opts)?;
    let mut jar: string = "";
    let mut page: HttpClientResp = r1;
    if r1.status == 302 {
        let loc = http_header_get(r1.headers, "location")?;
        jar = jar_add(jar, r1.headers);
        let g2 = http_do(HttpClientReq { method: "GET", url: loc, headers: jar_headers(jar), body: b, ca_pem: "" }, opts)?;
        if g2.status != 200 {
            return Err("login page status");
        } else {
        }
        page = g2;
    } else {
        if r1.status != 200 {
            return Err("auth endpoint status");
        } else {
        }
    }
    jar = jar_add(jar, page.headers);
    let html = http_bytes_to_text(page.body)?;
    let action = form_action(html)?;
    println("sso-form=ok");
    let form = "username=" + oidc_pct_encode(user)? + "&password=" + oidc_pct_encode(password)?;
    let fb = http_text_to_bytes(form)?;
    let mut fh = jar_headers(jar);
    push(fh, HttpHeader { name: "Content-Type", value: "application/x-www-form-urlencoded" });
    let r3 = http_do(HttpClientReq { method: "POST", url: action, headers: fh, body: fb, ca_pem: "" }, opts)?;
    if r3.status != 302 {
        return Err("login post status");
    }
    println("sso-login=ok");
    let loc3 = http_header_get(r3.headers, "location")?;
    let code = qparam(loc3, "code")?;
    if qparam(loc3, "state")? != "state-1" {
        return Err("state mismatch");
    }
    println("sso-code=ok");
    let toks = oidc_exchange_code(cfg, client, secret, code, redirect, pkce[0], "")?;
    let jwks = oidc_fetch_jwks(cfg, "")?;
    let raw_parts = split(toks.id_token, ".");
    let raw_payload = jwt_ascii_string(jwt_b64_decode(raw_parts[1])?)?;
    let exp = json_as_int(json_object_get(json_parse(raw_payload)?, "exp")?)?;
    let iss = base + "/realms/" + realm;
    let payload = oidc_verify_id_token(jwks, toks.id_token, exp - 60, 60, iss, client)?;
    println("sso-verify=ok");
    let me = oidc_userinfo(cfg, toks.access_token, "")?;
    let who = json_parse(me)?;
    let sub = json_as_string(json_object_get(who, "sub")?)?;
    let psub = json_as_string(json_object_get(json_parse(payload)?, "sub")?)?;
    if sub != psub {
        return Err("userinfo sub mismatch");
    }
    if sub == "" {
        return Err("empty subject");
    }
    println("sso-userinfo=ok");
    return Ok("sso-ok");
}

fn main() -> int {
    let want: Result<string, string> = Ok("sso-ok");
    let r = run();
    if r == want {
        return 0;
    } else {
    }
    println(r);
    return 1;
}
"#;

#[test]
fn oidc_password_grant_and_verify() {
    let src = PROG
        .replace(
            "%%BASE%%",
            &env("TOTO_TEST_OIDC_BASE", "http://127.0.0.1:8080"),
        )
        .replace("%%REALM%%", &env("TOTO_TEST_OIDC_REALM", "toto"))
        .replace(
            "%%CLIENT%%",
            &env("TOTO_TEST_OIDC_CLIENT_ID", "refsvc-rs256"),
        )
        .replace(
            "%%SECRET%%",
            &std::env::var("TOTO_TEST_OIDC_CLIENT_SECRET").expect(
                "TOTO_TEST_OIDC_CLIENT_SECRET must be set (provisioned per run, never committed)",
            ),
        )
        .replace("%%USER%%", &env("TOTO_TEST_OIDC_USER", "alice"))
        .replace(
            "%%PASSWORD%%",
            &std::env::var("TOTO_TEST_OIDC_PASSWORD").expect(
                "TOTO_TEST_OIDC_PASSWORD must be set (provisioned per run, never committed)",
            ),
        )
        .replace(
            "%%REDIRECT%%",
            &env("TOTO_TEST_OIDC_REDIRECT", "http://127.0.0.1:18080/cb"),
        );
    let outcome = run_src(&src);
    // The program returns 0 only on the exact "oidc-ok" prefix
    // path (sub check inside `run` would fail otherwise).
    assert!(
        outcome.result.is_ok(),
        "{:?} stdout={:?}",
        outcome.result,
        String::from_utf8_lossy(&outcome.stdout)
    );
    assert_eq!(
        outcome.stdout, b"",
        "stdout must stay empty on the green path"
    );
}

/// §6 browser-leg SSO: the full authorization-code flow with
/// form login (separate run, same provider). The program prints
/// fixed step lines only — no tokens, codes, or secrets ever
/// reach stdout — so the gate asserts the exact step sequence.
/// Any deviation fails loudly with its step.
#[test]
fn oidc_auth_code_form_login() {
    let src = PROG_SSO
        .replace(
            "%%BASE%%",
            &env("TOTO_TEST_OIDC_BASE", "http://127.0.0.1:8080"),
        )
        .replace("%%REALM%%", &env("TOTO_TEST_OIDC_REALM", "toto"))
        .replace(
            "%%CLIENT%%",
            &env("TOTO_TEST_OIDC_CLIENT_ID", "refsvc-rs256"),
        )
        .replace(
            "%%SECRET%%",
            &std::env::var("TOTO_TEST_OIDC_CLIENT_SECRET").expect(
                "TOTO_TEST_OIDC_CLIENT_SECRET must be set (provisioned per run, never committed)",
            ),
        )
        .replace("%%USER%%", &env("TOTO_TEST_OIDC_USER", "alice"))
        .replace(
            "%%PASSWORD%%",
            &std::env::var("TOTO_TEST_OIDC_PASSWORD").expect(
                "TOTO_TEST_OIDC_PASSWORD must be set (provisioned per run, never committed)",
            ),
        )
        .replace(
            "%%REDIRECT%%",
            &env("TOTO_TEST_OIDC_REDIRECT", "http://127.0.0.1:18080/cb"),
        );
    let outcome = run_src(&src);
    assert!(
        outcome.result.is_ok(),
        "{:?} stdout={:?}",
        outcome.result,
        String::from_utf8_lossy(&outcome.stdout)
    );
    let out = String::from_utf8_lossy(&outcome.stdout);
    for step in [
        "sso-discover=ok",
        "sso-form=ok",
        "sso-login=ok",
        "sso-code=ok",
        "sso-verify=ok",
        "sso-userinfo=ok",
    ] {
        assert!(out.contains(step), "missing step {step}:\n{out}");
    }
}
