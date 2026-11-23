Wave 3 verdict: ACCEPTED-WITH-FLAGS — merged to main as 3eb0c75. Database drivers + pooling + migrations + auth + config/secrets shipped; F1 concurrent-TLS deadlock diagnosed (any pool job blocking on a peer action wedges when the producing job is queued behind it) and fixed with 5 s TLS bounds + role-split lanes. Verdict, the auth API surface, and the flags approved by the user 2026-11-23. Gate green (74 result lines, 269 tests, 0 failed); CI green on run PENDING-STEP-3 (check + release-gates + bench + macOS/Linux ARM64 probes; Windows probe informational under continue-on-error). Details in the sections below; earlier section text is preserved verbatim.

# Enterprise Wave 3 report — data and identity

Status: MERGED TO MAIN as 3eb0c75 — Wave 3 ACCEPTED-WITH-FLAGS (verdict + F1 re-proof + CI green all recorded below). Earlier section text is preserved verbatim; the report is appended stage by stage (the STAGE 0 header above is superseded by the verdict + F1 sections, kept for the audit trail).

Branch: `enterprise/wave-3-data-identity`, from `main` at `34d2394`.
Never force-push; stage named files only (`git add`, never `-A`).
No merge until the wave gates green.

PDF gate: v2.5 roadmap verified this turn — title exactly "Toto
(.gzv) Implementation Roadmap v2.5 and Phase 0 Decision Doc (D1-D8)",
41 pages (`pdfinfo`), md5 `819bece4eb63aa13cd88f164bad94b75`
(same bytes as the recorded v2.5 document).

## Stage 0 — starting state + carry-forward disposition

Starting state verified this turn: `main` at `34d2394`;
`./scripts/check.sh` exit 0 (66 result lines, 240 passed, 0 failed);
CI run 44915756249 green (all jobs green except the Windows probe
job, failing visibly under `continue-on-error` — expected and
informational).

Standing authority carried in: D1–D8 FROZEN; 27 builtins frozen;
Phase 5 module model APPROVED as-is; ADR-009 + spec §17/§18
APPROVED as written 2026-11-21; Wave 0 policies APPROVED
2026-11-19; Wave 2 ACCEPTED-WITH-FLAGS merged (two framework v1
deviations + metrics flag accepted as written 2026-11-22 — Wave 3
builds on the framework as-is, no redesign, no silent fixes).
Windows DEFERRED WITH EVIDENCE; SUPPORTED platforms only
(Linux x86_64, macOS ARM64, Linux ARM64). `toto dev`
single-file-only stays a known open limitation.

Carry-forward dispositions (nothing disappears silently):
(a) E0216 race root-cause (`rebuild_cancels_parked_generation`,
Phase 4 JIT dev-mode settle — closed empirically, NOT
root-caused) — CARRIED, Phase 4 follow-up, not this wave.
(b) Intermittent CI stall (recurred on main) — CARRIED by
name; not fixed here unless it blocks the gate.
(c) SIGBUS per-program log-text confirmation — CARRIED as
an open item; diagnosis rests on repro + measurement +
the green re-proof run, not claimed as log-confirmed.
(d) TOML empty-table reporting — CARRIED (stdlib scope,
needs its own proposal).
(e) Native AOT net/crypto/gzip (JIT/AOT refuse by design)
— CARRIED as designed.
(f) Crypto/compression on Windows — CARRIED (follows the
POSIX-compat decision, not this wave).
(g) All-interfaces bind + Group C items (raw sockets,
packet work, full bytes story, WebSockets, HTTP/2,
security-toolkit libs) — CARRIED (separate gated
proposals, NOT this wave).
(h) The 8.6 KiB wording in the Wave 2 second addendum —
RECORDED in the merge-hygiene notes; cosmetic, the report
is not re-opened.
Wave 2 report follow-ups carried by name with the same
rationale (out of this wave's scope): rendezvous-latency
measurement; packaged differential coverage; GC past 100k
+ minimal-tunable-set; policy approvals; release
practice; per-platform matrix; dispatch-404s;
reproducibility-compare; multi-worker scheduler audit
follow-ups.

## §7 acceptance-criteria checklist (honest, updated per stage)

1. Starting state verified; follow-ups dispositioned — DONE
   (Stage 0 above; F1 added 2026-11-23, everything carried by name).
2. Database drivers + pooling — DONE (Stage 1 record; interop green
   in the fresh full gate: db_sqlite 7, db_postgres 4, db_mysql 4,
   db_pool 3).
3. Migrations — DONE (Stage 2 record; proj24 green in the gate).
4. Auth — DONE, flagged for acceptance review (Stage 3 record;
   ldap_auth 2 + oidc_keycloak 2 green locally and in CI run
   45116580699).
5. Config + secrets — DONE (Stage 4 record; test_config +
   test_secrets green incl. live Vault; no credential committed).
6. Reference service + load proof + SSO demo + gates — DONE
   (concurrent Phase C: 60-req conc=2 `LOAD-OK` locally with
   published numbers + methodology; local gate CHECK_EXIT:0 at
   74/269/0; CI run 45156987879 Success with check +
   release-gates + bench + macOS/ARM64 probes green, Windows
   probe the only failure, informational). Sequential-only
   numbers did NOT close this criterion — the concurrent
   re-proof above does.
7. Report complete — DONE (records + totals + packaging note +
   follow-ups + verdict + CI-proof note below).

### Stage 1 implementation record

Implemented on this branch: `crates/toto-compiler/src/dblib.rs`
(new; pool + three drivers + framing), `build.rs` extended
(sqlite3 unix-wide via `TOTO_DB_LIB_DIR`-prefixed search;
libpq/libmysqlclient Linux-only, matching the
`cfg(target_os = "linux")` modules), `ffi.rs` (5 linked
names, POSIX_ONLY on Windows, per-task `db_error` slot,
interpreter arms), `sched.rs` (`BlockingOp::Db`/`DbOp`,
`BlockingResult::DbInt/DbRows/DbFail`, drain pre-pass +
`db_error` slot, `run_db` on the pool), `exec.rs`
(`DB_FNS`, `exec_db`, `db_wait` with the same
check_deadline/maybe_collect/trace discipline as TLS),
`compile.rs` (JIT refuses `toto_rt_db_*` like the other
pool surfaces — executor is the reference; AOT shares the
refusal), `stdlib/db.gzv` + `stdlib/tests/test_db.gzv`.

Decisions made while implementing (all within the recorded
design): (1) the interpreter and runtime engines share ONE
framing string both ways (params in, rows out as a string
decoded by pure Toto) — a single wrapper, a single decoder,
no engine divergence; non-finite floats are a loud `Err` on
the interpreter path only where Toto cannot construct them.
(2) DDL affected counts are driver-defined (documented —
assert success, not the count). (3) MySQL decodes by value
shape (no column types on the text path) with the
documented wart; Postgres numerics arrive as text
(precision kept); MySQL `TEXT "123"` reads as int —
CAST/state accordingly. (4) `mysql_server_init` (the real
symbol behind the macro) serializes first-use init once —
concurrent pool-thread connects raced it (observed as
spurious `init: allocation failed` under parallel tests).
(5) Postgres NOTICEs are swallowed per connection (a
no-op notice processor — goldens stay clean). (6) Corpus
DB cases live under `tests/corpus/project/` (proj21–23:
basic, file, concurrent hammer) — the differential
harness runs only single-file top-level programs, and DB
cases need imports, so they follow the project pattern
(single-file `database/` area rejected for exactly this
reason). (7) Pool exhaustion/timeout/cancellation live in
the Rust suite (`toto-runtime/tests/db_pool.rs`,
deterministic: exhaustion by overlapping holders with an
order-independent aggregate, cancellation via scope
fail, hammer counts), not in corpus goldens (no timing in
goldens). (8) Transactions are per-statement in v1 — no
held-connection API, so a mid-batch failure leaves
earlier statements committed (proven by test); Stage 2
designs around this with a ledger + down-migrations.

Proven this turn (local, Linux x86_64): SQLite (both
engines, `:memory:` + file, multibyte/quoting), Postgres
16 + MySQL 8.0 in docker (CRUD, types, params, errors,
transactions-are-per-statement). CI provides the same via
service containers + apt client libs (`check` job only —
the macOS probe needs no database: corpus DB is
SQLite-only). No credentials committed (test-only
passwords in ephemeral containers + CI env, never in
fixtures or goldens).

§7 checklist: criterion 2 — IN PROGRESS (drivers + pool
shipped and proven locally; CI interop pending push).

### Stage 2 implementation record

Library `gzv.std/migrate` (pure Toto over `gzv.std/db` —
no new FFI, no CLI in v1). Insertion-sort by version +
duplicate rejection; ledger `toto_schema_migrations`
(`IF NOT EXISTS`); apply-in-order with stop-at-first-failure
(resumable); rollback strictly above target in descending
order with the ledger trimmed per success; empty `down`
means irreversible (loud named `Err`); rolling back the
unapplied is a no-op `Ok`. Proven by `stdlib/tests/
test_migrate.gzv` (apply/rollback/failure-resume/
duplicates/irreversible, ledger verified at every step)
and corpus `proj24_db_migrations` (out-of-order input,
schema+data+ledger goldens before/after, irreversible
refusal pinned). Per-statement atomicity only (Stage 1
limitation carries explicitly — proven by the pg
`transactions_are_per_statement` test).

§7 checklist: criterion 3 — IN PROGRESS (shipped and
proven locally; CI proof pending push).

## Stage 4 — config + secrets design (recorded before implementation)

Config (`gzv.std/config`): files (TOML/YAML via the Wave-1
serialization formats), environment, and inline defaults —
precedence env > file > defaults (stated, tested). One
value model: a `JsonVal` object mapping dotted paths to
STRINGS (TOML/YAML trees flatten on load, arrays as
`a.0`-style segments; typed getters parse with loud
`Err`s). Env comes from two tiny libc FFI fns
(`toto_rt_env_get`/`toto_rt_env_has` — FFI plumbing like
everything this wave, not new builtins) via an explicit
`"ENV_NAME=config.path"` mapping list (predictable, no
magic prefix rules). All config tests use fixtures +
explicit env in the harness (never ambient CI env).

Secrets (`gzv.std/secrets`): store-agnostic
`secrets_fetch(store, key)` / `secrets_rotate(store, key,
value)` where `store` names the backend (`vault` |
`file`). `vault` is the REAL HashiCorp Vault over HTTP
(KV-v2 read/write via the Wave-2 http client; rotation =
new version write; old versions retained per Vault;
failures surface HTTP status + body). `file` is a
file-backed double with identical fetch/rotate semantics
(including rotation-failure behavior) that proves the
rotation LOGIC. Cloud secret stores (AWS/GCP): NO live
access from here or CI — stated gap, never asserted; the
`file` double + the store-agnostic seam are the extension
point, and the report says so explicitly. Provenance:
Vault dev server in docker locally + as a CI step (real
server, test-only root token via env); fixtures carry
placeholders only. Credential rule, precisely: NO
production, long-lived, or cloud credential anywhere in
the repo, ever. Test-only passwords for ephemeral
localhost/CI services (test DBs/LDAP, test Keycloak
admin/user, Vault dev root) MAY appear as committed
test defaults and CI workflow env (industry standard —
GitHub's own docs do this; they unlock nothing outside
their throwaway instances, which are created fresh per
run with exactly those values). The OIDC client secret
and user password are env-required with no committed
default (higher sensitivity); Vault's dev root token is
env-only.

## Stage 3 — auth API design (recorded before implementation;
user reviews the surface at acceptance — flagged here so it
is not approved by inattention)

Modules (all pure Toto except the two noted FFI additions;
no new builtins, no D5/type-system change, no checker
change):
- `gzv.std/password`: `pw_hash(password) ->
  Result<string>` (PBKDF2-HMAC-SHA256, 600k iterations
  default per OWASP 2023, 16-byte random salt) and
  `pw_verify(password, stored) -> Result<bool>`. Format
  `$toto$pbkdf2-sha256$i=<n>$l=<b64url salt>$<b64url hash>`.
  Backed by ONE new libcrypto binding
  (`PKCS5_PBKDF2_HMAC` — the mature library already
  linked; same Wave-2 pattern as `pem_to_pkey`) plus a
  constant-time compare in native code; the KDF math
  itself is never Toto code (600k HMAC rounds in Toto
  would be minutes per hash). Inline crypto discipline
  (one-shot call); the ~0.3 s cost at default iterations
  is documented, async-offload is a recorded follow-up.
- `gzv.std/jwt`: base64url codec in pure Toto (an
  encoding, not crypto) + `jwt_sign_hs256` /
  `jwt_verify_hs256` (HMAC via Wave-1 bindings) +
  `jwt_sign_es256` / `jwt_verify_es256` (ECDSA P-256 via
  Wave-1 `sign_*`; keys are the 32-byte scalar /
  64-byte x||y shapes `sign_keygen` already produces).
  Verification enforces `alg` (rejects `none` and
  mismatches — the classic confusion), signature, and
  `exp`/`nbf`/`iat` with a caller-supplied skew.
  Pinned RFC 7515 A.1 HS256 vector. `jwt_verify_rs256`
  ships too (RSA PKCS#1 v1.5 via a new libcrypto binding —
  verify-only, keyed by JWK `(n, e)` bytes; the mature
  library, FFI-first, same Wave-2 precedent as the PEM
  helpers — NOT home-grown RSA). HS256 against Keycloak
  client secrets was abandoned with evidence (the IdP
  signs with an opaque derived key, not the raw secret —
  verified empirically twice); RS256+JWKS is the standard
  path and the one proven.
- `gzv.std/oidc`: discovery (`GET .well-known`), auth-URL
  builder (state/nonce/PKCE-S256 via Wave-1 SHA-256),
  code exchange (POST form), ID-token validation (JWT
  verify + `iss`/`aud`/expiry/nonce checks), userinfo
  fetch. Provider: Keycloak with HS256-signed ID tokens
  (client setting — the realm default RSA stays
  untouched). No browser is automated in CI: interop
  proves discovery + password-grant tokens + validation
  end to end; the §6 demonstration drives the full
  auth-code leg with form login over HTTP (same client).
- `gzv.std/saml`: `saml_parse_response` (Issuer/Subject/
  NameID/Audience/Conditions times/attributes) +
  `saml_validate_conditions` (time windows, audience).
  XML-DSig/RSA signature verification is an explicit
  STATED GAP (needs RSA bindings + XML canonicalization
  — a follow-up; fixtures carry placeholder signatures
  and NO keys are committed, per the Stage 4 rule).
- `gzv.std/ldap` + 3 FFI names (`toto_rt_ldap_bind`,
  `toto_rt_ldap_search`, `toto_rt_ldap_last_error`) over
  system libldap (Linux-only like pq/mysql — no system
  libldap the probe can link; E0301 elsewhere).
  Each op connects+binds+operates+unbinds on the
  blocking pool (same policy as DB — never a worker);
  no handle cache in v1 (documented). Search returns a
  JSON array string parsed by `gzv.std/json` (no new
  framing — JSON is the Wave-1 carrier). Interop against
  OpenLDAP in docker (bind + subtree search + filter).

What ships as what: OIDC is the demonstrated SSO (§6,
Keycloak, external IdP); LDAP ships live-tested
(OpenLDAP); SAML ships as surface + structural tests
with its signature gap stated — never claimed as
validated auth. No home-grown crypto primitive, mode, or
validation logic anywhere: every signature/hash goes
through the linked mature libraries.

## Stage 2 — migrations (design recorded before implementation)

Tooling: a library, `gzv.std/migrate`, over the Stage 1
`gzv.std/db` surface (no new FFI, no CLI in v1 — the
corpus + stdlib prove the library; a `toto migrate` CLI
is a follow-up, not smuggled in here).

Shapes: `Migration` is `{version:int, name:string,
up:string, down:string}`. Versions order application
(ascending); duplicates are an `Err`; gaps are allowed
(versions need not be contiguous — recorded, not
assumed). An empty `down` string means IRREVERSIBLE by
convention; attempting to roll it back is a loud `Err`
naming the version (a migration that cannot be rolled
back says so explicitly, at the attempt, never silently).

Ledger: `toto_schema_migrations (version INTEGER PRIMARY
KEY, name TEXT)`, created with `IF NOT EXISTS` on first
use. The ledger is the only state (no files, no checksums
in v1 — content drift after apply is out of scope and
stated as such).

Apply (`migrate_up(dsn, migrations)`): read applied
versions from the ledger; run each pending `up` in
version order via `db_exec`; record `(version, name)` on
success; STOP at the first failure and return its `Err`
(library message preserved). Schema state on failure:
all migrations before the failed one applied, the failed
one not recorded — deliberate, resumable (re-running
applies the remainder). No implicit transaction wraps
the batch (Stage 1 per-statement limitation carries over
explicitly — a single statement is still atomic in its
engine).

Rollback (`migrate_down(dsn, migrations, target)`):
applied versions above `target` roll back in DESCENDING
order via their `down` SQL, each removed from the ledger
after its `down` succeeds; a missing `down` (empty
string) or a failing `down` stops with a loud `Err` and
leaves the ledger exactly where the failure happened
(partial rollback is visible state, not silent drift).
Rolling back what was never applied is a no-op `Ok`.

Failure behavior summary: apply and rollback are
stop-at-first-failure with the library error surfaced;
the ledger always reflects exactly the applied prefix
(the applied set is always a prefix of the sorted
known-version list — no holes, because apply proceeds
in order and rollback in reverse). DDL-heavy migrations
inherit the
driver-defined affected counts (assert success, not
counts — Stage 1 rule).

## Stage 1 — database drivers + connection pooling

### Driver choices (FFI-first, no Toto-implemented drivers anywhere)

- SQLite — system libsqlite3 (Ubuntu `libsqlite3.so.0`,
  macOS `/usr/lib/libsqlite3.dylib`; both carry it as an
  OS library). Public-domain (blessing). Rationale:
  serverless and file-based, so the corpus proves it on
  every platform with no server (the macOS probe needs
  no database service); ACID, mature, most-deployed DB
  engine in existence. Bound API (all opaque-pointer +
  int, decades stable): `open_v2`, `prepare_v2`,
  `bind_int64/double/text/null`, `step`,
  `column_type/int64/double/text/bytes`, `finalize`,
  `close`, `errmsg`, `changes`, `last_insert_rowid`.
  No structs-by-value, no dlopen.
- Postgres — libpq (`libpq.so.5`, Ubuntu 18.x locally,
  CI via apt). PostgreSQL licence (permissive).
  Rationale: THE native driver, 30+ years mature,
  thread-safe connection objects (one conn per use,
  pool discipline holds). Bound API (all stable for
  20+ years): `PQconnectdb`, `PQstatus`, `PQexecParams`
  (server-side binding — no string interpolation),
  `PQprepare/PQexecPrepared`, `PQresultStatus`,
  `PQntuples/nfields/fname/ftype/getvalue/getlength/
  getisnull`, `PQclear`, `PQfinish`, `PQerrorMessage`.
  Blocking socket API → blocking pool (TLS/DNS
  precedent).
- MySQL — libmysqlclient (`libmysqlclient.so.24`,
  8.4.x locally, CI via apt). GPL-2.0-or-later with the
  Universal FOSS Exception — RECORDED as a deployment
  consideration (dynamic link; the exception covers
  FOSS; SQLite and libpq are unencumbered). Rationale:
  the mature native C API. Bound API (all stable):
  `mysql_init`, `mysql_options` (connect timeout),
  `mysql_real_connect`, `mysql_real_query`,
  `mysql_store_result`, `mysql_num_rows/fields`,
  `mysql_fetch_row/lengths`, `mysql_free_result`,
  `mysql_close`, `mysql_error/errno`. Parameters via
  the library's own `mysql_real_escape_string`
  (charset-aware, provided by the mature library — NOT
  Toto escaping); Postgres/SQLite use server-side
  binding. The difference is documented, not hidden.
  A `MYSQL_BIND`-struct prepared-statement path was
  rejected (struct layout drift across 8.x without
  headers on every machine — the Wave 1 addrinfo
  lesson: never hand-roll a layout that can drift).

Platform split: SQLite is unix-wide (system lib on
Linux and macOS); Postgres/MySQL are linux-only
(`cfg(target_os = "linux")`, clean check-time refusal
elsewhere — macOS ships no system libpq/libmysqlclient
and the probe builds without them). Corpus DB programs
use SQLite only (portable, serverless); Postgres/MySQL
interop lives in Rust suites on the Linux check job.

### Pooling design (recorded before implementation)

Location: a process-global pool in the new `dblib`
module (the SESSIONS/UDP_PEERS precedent), keyed by
(driver, DSN). Each pool: idle handles, total count,
max size, checkout timeout; one `Mutex` + `Condvar`
per pool. No scheduler state, no heap access from
pool threads — owned plain data only.

Blocking-I/O policy (explicit): NO database wait ever
runs on a scheduler worker or the poller. Checkout,
connect, query, and fetch ALL execute on the dedicated
blocking pool (the DNS/TLS/file pool,
`blocking_pool_threads`); the task parks on
`"blocking"` with its `blocking_seq` (existing
mechanism, stale-completion guards unchanged) and the
pool thread posts a `Db*` completion. New
`BlockingOp::Db` / `BlockingResult::DbInt/DbRows/
DbFail` variants (the `Tls` shape); the drain sets a
per-task `db_error` slot (never `net_error`/`tls_error`,
same separate-slot discipline).

Semantics (each tested): exhaustion — checkout waits up
to the configured timeout (default 5000 ms), then
`Err("pool exhausted (N connections in use, timeout
M ms)")`; the waiter is released, nothing leaks.
Timeouts — connect timeout (driver-level: libpq
`connect_timeout`, MySQL `MYSQL_OPT_CONNECT_TIMEOUT`;
SQLite open is instant) + checkout timeout
(pool-level). Per-query timeout is NOT provided in v1
(queries run to completion; limitation recorded, not
silent). Cancellation — a task cancelled while parked
is released with `Cancelled` through the standard sweep
(same as TLS waits); the in-flight query finishes on
the pool thread and its connection recycles to idle —
the task is freed immediately, no worker consumed, no
connection leaked, no poisoned handle reissued. A
cancelled checkout that already obtained a connection
returns it to idle.

Defaults and limits (documented, not silent): default
max 8 connections per pool, default checkout timeout
5000 ms, both configurable per DSN
(`db_pool_config`); no idle eviction in v1; one pool
per (driver, DSN); connections are one-task-at-a-time
(a checked-out handle never leaves its pool job).

Value model (no D5/type-system change): `DbVal` is a
plain Toto struct `{kind:int, i:int, f:float, s:string}`
(`kind`: 0 null, 1 int, 2 float, 3 text, 4 bool);
drivers decode int/float/bool/text by native type,
everything else arrives as text (documented). No blob
in v1 (recorded gap). Placeholders are `?` positional
in Toto SQL, translated per driver (Postgres: `?` →
`$n` outside string literals/comments via a small
tested scanner; MySQL/SQLite: native `?`). A query
that cannot be rolled back (DDL) says so at the
migrations layer (Stage 2), not here.

DSN shapes: `sqlite:path=/x/y.db` (plus
`sqlite::memory:`), `postgres:host=.. port=..
dbname=.. user=.. password=.. [connect_timeout=..]`,
`mysql:host=.. port=.. dbname=.. user=.. password=..
[connect_timeout=..]`. Secrets never live in goldens
(placeholders in fixtures; Stage 4 owns rotation).

### Stage 4 implementation record

Shipped on this branch: `stdlib/config.gzv` +
`stdlib/tests/test_config.gzv`, `stdlib/secrets.gzv` +
`stdlib/tests/test_secrets.gzv`, `crates/toto-compiler/src/
stdlib.rs` (+2 modules; the `stdlib_table_is_complete` pin
moves 27 → 29 with this stage: 21 at the Stage-1 tip + 6
from Stages 2–3 + 2 here),
`crates/toto-compiler/src/netsys.rs` (`env_get` over libc
`getenv`, inline/never-blocks),
`tests/corpus/diagnostics/diag13_ffi_unlinked.stderr`
(golden gains `toto_rt_env_get/has` — registered in Stage 3
but the golden lagged one step), `docs/stdlib.md`
(`config` + `secrets` sections),
`.github/workflows/ci.yml` (Vault dev-server step +
`VAULT_ADDR`/`VAULT_TOKEN` for the `check` job),
`scripts/ci-provision-identity.sh` (Vault section:
`docker run` dev server on `:8200`, health-gated,
KV-v2 `secret/` seeded with placeholder keys).

Decisions made while implementing (all within the recorded
design): (1) the merge needs no schema knowledge — the
flat dotted-string model makes `config_merge` a per-key
overlay (later wins), so layered config is trivially
correct. (2) Env overlay maps are explicit
`"ENV=config.path"` strings (no prefix magic); unset
variables are SKIPPED (never blanked in); a mapping
without `=` is a loud `Err`. (3) Empty-vs-unset env
distinguishes through `config_env_has` (`getenv` returns
`""` for both) — an empty password is a misconfig `Err`,
never a credential. (4) The Vault name grammar is
`mount/path[#field]` (default field `value`); the write
path returns the KV-v2 version NUMBER rendered to string
(Vault versions are JSON numbers — `json_as_int` then
`to_string`, found live: `json_as_string` on the version
`Err`d). (5) `secrets_fetch_first` is fail-CLOSED: sources
are skipped only when provably absent (bool presence
probe — unset env, missing file, unconfigured vault),
never on read failure; a present source that fails
propagates loudly instead of falling through (no silent
fallback to a weaker source). (6) Cloud names
(`aws`/`gcp`/`azure`) are explicit unavailable providers
behind the same two functions — the seam ships, the
drivers stay a stated gap. (7) `toto fmt --check` is NOT
run on `stdlib/*.gzv` (committed Wave-1 files fail it
too — pre-existing drift, not a gate); new modules match
the surrounding hand style instead.

Bugs found while proving (fixed, with evidence): two
infinite loops in the first flatten pass (the `while`
index never advanced — hung `toto run` until killed);
TOML scalar arrays need `toml_table_get_array`
(`table_get` `Err`s on `[...]` literals — dispatch on
the raw literal); root-level arrays flattened to `.0`
keys (`config_join` skips the dot on an empty prefix);
`config_get`'s default was unreachable (`json_object_get`
`Err`s on missing — pre-check with `config_has_key`);
`?` needs a `Result` return (probes use plain `main`
plus inner fns); `netsys.rs` needed `use std::ffi::CStr`
plus `#[link_name = "getenv"]` (first placed under the
`getaddrinfo` attribute — caught at link, fixed before
any commit).

Proven this turn (local, Linux x86_64): `test_config`
green (loaders incl. a real file under `target/`, merge
precedence with inputs unchanged, env overlay with
`PATH` + unset-skip + bad-mapping rejection, typed
getters incl. bad-int/bad-float/non-finite/bad-bool);
`test_secrets` green WITHOUT a server (closed-door
assertions) AND against a local Vault 1.15 dev server
(real KV-v2 fetch + rotate with version advance +
restore, vault-wins cascade, bad-field/bad-shape loud).
Vault 1.15 serves `data.data` exactly as parsed; writes
return the version under `data.version`. No credential
committed: every value is `placeholder-*` or ambient
(`PATH`); the dev root token is env-only. CI provides
the same via the provisioned dev server (`check` job
only — probes need no secrets).

Design deltas (explicit, no silent drift): (i) the
recorded design named stores `vault | file`; shipped
providers are `env | file | vault` plus named-gap
`aws/gcp/azure` — `env` is required for the overlay
story and the local-dev fallback, and naming the cloud
gap beats silence. (ii) The design said "never ambient
CI env" for config tests; one ambient read remains
(`PATH` — env cannot be set from inside the language,
so no hermetic positive overlay proof exists; the
value is never asserted, only self-consistency).
(iii) New carried Wave-1 follow-up (same class as the
TOML empty-table item): an unquoted YAML scalar ending
in `:` re-parses as a mapping head (pinned by
`test_yaml.gzv:75` — out of this wave's scope to fix);
quote such values (shown in code + docs + tests).

§7 checklist: criterion 5 — IN PROGRESS (shipped and
proven locally incl. live Vault; CI proof pending push).

### Stage 3 implementation record (appended 2026-11-23, Stage-5 turn)

Shipped on this branch: `stdlib/jwt.gzv` + `stdlib/tests/test_jwt.gzv`,
`stdlib/password.gzv` + `stdlib/tests/test_password.gzv`,
`stdlib/ldap.gzv`, `stdlib/saml.gzv` + `stdlib/tests/test_saml.gzv`,
`stdlib/oidc.gzv`, `crates/toto-compiler/src/ldaplib.rs` (native LDAP
surface over system libldap), `cryptolib.rs` PBKDF2-HMAC-SHA256
(`PKCS5_PBKDF2_HMAC`) + RSA verify-only bindings, `ffi.rs`
registrations (`toto_rt_crypto_*`, `toto_rt_ldap_*`), `lib.rs`,
`crates/toto-jit/src/compile.rs`, runtime `exec.rs`/`sched.rs`
(blocking-pool paths), `crates/toto-runtime/tests/ldap_auth.rs`,
`crates/toto-runtime/tests/oidc_keycloak.rs` (password-grant +
auth-code-form-login SSO demo), `tests/fixtures/ldap/seed.ldif.tmpl`,
`scripts/ci-provision-identity.sh`, `stdlib/crypto.gzv` (+41 lines),
`docs/ffi.md`, `build.rs` link updates. [Pre-push amendment
2026-11-23: `docs/ffi.md`, `ci.yml`, and the provision script also
carry Stage-4 additions in the committed bytes — see the packaging
note below; nothing is claimed here that the files do not contain.]

Proven: Keycloak RS256/JWKS — password grant AND full auth-code leg
with automated form login, against real Keycloak 26 (local docker;
CI-services leg pending push). HS256-against-client-secret abandoned
with evidence (the IdP signs with an opaque derived key, not the raw
secret — verified twice); RS256+JWKS is the standard path. OpenLDAP
bind + subtree search + filter (docker, seeded by the provision
script; ephemeral test-only credentials). SAML structural parsing +
conditions validation (XML-DSig/RSA verification an explicit STATED
GAP — fixtures carry placeholder signatures, no keys committed).
PBKDF2-HMAC-SHA256 at 600k iterations default per OWASP 2023
(~0.3 s per hash — documented; async offload a recorded follow-up);
no home-grown crypto anywhere. RFC 7515 A.1 HS256 vector pinned.

Design deltas vs the recorded Stage-3 design: (i) RS256+JWKS shipped
as the proven path where the design recorded an HS256 client-secret
option — abandoned with evidence, never silently. (ii) The SSO demo
automates the form POST over plain HTTP (no browser involved — the
design constraint honored, fuller leg than password-grant-only).
(iii) `oidc_exchange_code` gained a `code_verifier` parameter
(required when the auth URL used a PKCE challenge; empty string
omits it for plain flows) — additive, same function, documented
here.

Auth API acceptance: the surface is exactly the five modules plus
the OIDC discovery/auth-url/exchange/verify/userinfo shapes in the
design section above — flagged here for the user's acceptance
review, not approved by inattention. No new builtins, no D5/type-
system change, no checker change.

### Stage 5 implementation record (reference service + load + benchmarks; appended 2026-11-23)

Location: `tests/corpus/project/proj25_refsvc_data/` — `common.gzv`
(service core shared by both entry points), `main.gzv`
(deterministic golden), `load.gzv` (load hammer), `fixtures/`
(loopback TLS PKI valid to 2056, `service.toml` topology config —
no secrets anywhere). Rust gate:
`crates/toto-runtime/tests/refsvc_load.rs` (SQLite/PG/MySQL variants
via DSN env + cancel program).

Service: TLS loopback via the Wave-2 HTTP framework over the Wave-1
substrate; SQLite file DB in the case dir (hermetic on every
platform, serverless) and file/server DSNs in the load runs;
migrations at startup (Stage-2 library); two seeded users with
PBKDF2 hashes (Stage-3 password, fresh random passwords per run);
HS256 Bearer JWT (Stage-3 jwt, per-process random key — no committed
secret, no env); structured logging + Prometheus exposition (Wave-2
substrate, proj20 patterns). Auth: POST /login mints the token;
every other route verifies signature + iss, then enforces the
monotonic-ms TTL (v1 has no wall clock). Structural failures are
401s; forgeries abort fail-closed (accepted v1 deviation — the
service never returns Err). Deliberately out of scope: no
registration, no refresh, no per-query timeout, no browser UI.

Golden (`main.gzv`, corpus): login ok / bad-password + unknown-user
401s / no-token, bad-scheme, bad-shape 401s / forged-abort /
expired-401 / authed read 200 / unknown-user 404 / two authed
writes + read-back / gated metrics 401 + byte-exact scrape match /
wrong-CA rejection + survival / normalized log drain + count. Fixed
1ms latency stand-in, `ts` normalized, random secrets never printed.

Pool behavior under load, verified (methodology + numbers):
Phase A — forced exhaustion, 8 concurrent direct-DB tasks on a slow
query, pool max 1 / checkout timeout 300 ms. Max sits BELOW the
blocking-pool thread count (2): running jobs can never exceed the
thread count, so a max >= threads can never exhaust (checkout always
finds capacity); max 1 forces the documented timeout path. Gate:
every attempt is Ok or the documented `pool exhausted...` Err, at
least one exhausts (the slow query outlasts the timeout by an order
of magnitude — same margin class as `db_pool`'s SRC_EXHAUST).
Measured (local, debug interpreter): ok=1, exhausted=7, other=0
(SQLite + Postgres); ok=2, exhausted=6, other=0 (MySQL — same gate,
counts vary by holder timing). MySQL needs its own slow query
(SELECT SLEEP(2)): the recursive CTE aborts fast at 1000 iterations
(errno 3636). Phase C — sequential service load: one login, then 60
authed reads back to back on a single connection (sequential by
explicit design — concurrent TLS requests whose handlers park on
the blocking pool deadlock the shared pool; see follow-up F1
below). Each request is timed on the monotonic clock; every
response must byte-match the exact expected shape; any deviation
fails loudly with its request index. Measured (same setup, 3 full
runs): 60 reqs, elapsed ~61-71 s, ~0.84-1.0 rps, p50 ~1010-1080 ms,
p99 ~1010-1070 ms typical with one ~2 s outlier observed (n=60 per
run). Per request ~1 s interpreted (routing + header parse + JWT +
DB framing in Toto). CI bench row measures wall time of the whole
run; RPS/latency are deployment data, never pinned. PG/MySQL
variants run the same gate (10-req hammer in the Rust suite for
time; behavior classes identical). Recovery: db_close_pool then
fresh query ok. Cancellation: separate program (max-1 pool) —
waiter on slow query under sibling burst, scope cancelled, E0214
through the standard sweep, then same-process pool serves a burst
all-ok (asserted at the Rust level; no hang under contention).
Timeouts: checkout timeout proven by phase A (message names the
configured pool); connect timeout is driver-level (exercised by
every CI run against real PG/MySQL); per-query timeout is NOT
provided in v1 (recorded limit). Load benchmarks, published:
RPS ~0.95 (single-connection sequential service load, debug
interpreter, loopback TLS, x86_64); latency p50 ~1015 ms,
p99 ~1040 ms. GC pauses (release `gc_perf`): 1k live med 43us
p99 53us; 10k med 294us p99 313us; 100k med 5.5ms p99 6.5ms — all
under the 10 ms Wave-0 budget. Bench: `scripts/bench.sh` gains
`exec-refsvc-load` (correctness gate plus timed median vs 5x
baseline rule). Baseline `exec-refsvc-load 66496` (wall-ms of one
60-req sequential run on the reference x86_64 host).
Wave-2 follow-up F1 (load-bearing, NEW, carried as OPEN): concurrent
TLS requests whose handlers park on the blocking pool deadlock the
shared 2-thread pool (order-dependent; minimal repro: two concurrent
`http_do` GETs against a `http_serve_tls` with a DB or file-blocking
handler — hangs indefinitely). Plain-HTTP same shape completes;
sequential TLS completes; concurrent TLS without blocking waits
completes. Suspected mechanism: client-side and server-side TLS
handshake/record jobs pile on the same side of the pool (mutual wait
on network reads for flights only the queued counterpart can
produce), no timeout, invisible to the E0216 deadlock guard. Not
proven with pool introspection (none exists). Impact: concurrent
service load deferred; contention proven directly; service numbers
are single-connection. No Wave-3 code works around it (that would
paper over a gate-relevant defect).
SSO demonstration: `oidc_auth_code_form_login` in
`crates/toto-runtime/tests/oidc_keycloak.rs` — discovery, auth URL
(PKCE S256), login form, credential POST with manual cookie jar,
code extraction, code exchange (with verifier; `oidc_exchange_code`
gained an additive `code_verifier` param), RS256 JWKS verify,
userinfo with sub match. Fixed step lines only (no tokens, codes,
or secrets). Green against real Keycloak 26 locally; CI leg
pending push. [Pre-push amendment 2026-11-23: the draft line
"locally and in CI" is struck until the push proves it.]
Isolation finding: the golden first used `sqlite::memory:` but
`fmt_equiv` runs all corpus projects in one process (shared
process-global pools), so its `users` table and ledger versions
collided with other cases' tables; the fixture now names a
case-local file DB (`refsvc_data_tmp.txt`, cleaned by the harness
like proj22's), which is byte-identical in output and hermetic
everywhere. MySQL DDL finding: the users-table PRIMARY KEY on TEXT
fails on MySQL/utf8mb4 (errno 1170); the migration uses
VARCHAR(191) (affine TEXT on SQLite, varchar on Postgres) —
portable everywhere, recorded here. Minor wart (follow-up, not
fixed here): `db_exec` of a result-returning statement on MySQL
returns an empty error string (error slot unset on that path)
instead of naming the misuse.

### Fresh full-gate totals (this turn: local detached run 2026-11-23, CHECK_EXIT:0)

Harness: `/tmp/checkall.sh` (detached, `TOTO_DB_LIB_DIR` +
`LD_LIBRARY_PATH=$HOME/.local/lib/toto-deps`; services local —
PG :5432, MySQL :3306, OpenLDAP :53389 seeded by
`scripts/ci-provision-identity.sh`, Keycloak :54080 provisioned
realm `toto`, Vault dev :8200). `cargo fmt --all -- --check`
clean; `cargo clippy --workspace --all-targets -- -D warnings`
clean; `cargo test --workspace` green: 73 result lines, 266 passed,
0 failed (Wave 2 was 66 / 240 / 0; delta +7 lines / +26 tests =
exactly the new suites: `db_sqlite` 7, `db_postgres` 4, `db_mysql`
4, `db_pool` 3, `refsvc_load` 4, `oidc_keycloak` 2, `ldap_auth` 2).
Per-binary passed (all failed 0; same-named binaries across crates
merged): unittests src/lib.rs 34, cli 17, unittests src/main.rs 16,
project 15, gc 11, dev 11, reload 10, packages 10, jit 8, interp 8,
gate 8, spike1 7, lexer 7, ffi 7, db_sqlite 7, stack1b 6, mn_jit 6,
parser 5, mn 5, refsvc_load 4, http_interop 4, db_postgres 4,
db_mysql 4, blocking 4, tls_openssl 3, tls_interop 3, opt 3,
lsp 3, gc_perf 3, db_pool 3, concurrent 3, stack_jit 2, race 2,
oidc_keycloak 2, ldap_auth 2, fmt 2, differential 2, ctx_stage3 2,
1 each: corpus, differential_concurrent, differential_task,
fmt_equiv, mir, mir_conform, net_hammer_mw, panics, sequential,
stack, timings, yield_jit, zz_ffidiag (sums to 266 — verified).
Corpus: stdlib `toto test` 24 passed 0 failed (was 17; +7 new
suites: config, db, jwt, migrate, password, saml, secrets);
wordfreq `toto test` 1 passed + interp==built identity. Corpus
census: 186 cases (160 top-level `.gzv` — concurrent 22,
diagnostics 16, interp 30, lexing 5, parsing 9, results 13,
typing 65 — plus 26 project `main.gzv`s, proj01–proj25 incl. both
proj11 variants; was 181 = 160 + 21). No overrides needed on this
host (no TMPDIR/RUST_MIN_STACK workarounds; macOS-only guards
untouched — CI proves the other platforms).

### Commit-packaging note (dated 2026-11-23; history hygiene, no content change)

Stages 2–5 are committed as four sequential commits (Stage-2 /
Stage-3 / Stage-4 / Stage-5) on top of `4026697` (Stage 1),
because the intermediate worktree was one uncommitted union and
the report file is shared append-only prose: carving it per commit
would rewrite committed text. Each commit message lists exactly
what it carries. Three deliberate packaging calls, recorded here
so the history reads honestly: (1) `ffi.rs` + `exec.rs` ship whole
in the Stage-3 commit although they contain the Stage-4 env-inline
lines — the executor's extern dispatch and the allowlist were
extended once for both stages' externs, and `netsys.rs` (`env_get`)
rides in the Stage-3 commit as the dependency that keeps it
compiling; Stage-4 code that USES those lines (config overlays)
lands in the Stage-4 commit. The Stage-4 record's parenthetical
"(registered in Stage 3 but the golden lagged one step)" is
accurate under this packaging. (2) `ci.yml` + the provision script
ship whole in the Stage-3 commit although the Vault dev-server
section and the two `VAULT_*` env lines serve Stage 4 — they ride
along inertly (no Stage-3 test reads `VAULT_*`); where the
Stage-4 record says "shipped", read "first used", with the bytes
arriving one commit earlier as documented here. (3) Progressive
pins: `stdlib.rs` carries 22 modules in the Stage-2 commit
(+migrate), 27 in Stage-3 (+jwt/password/ldap/saml/oidc),
29 in Stage-4 (+config/secrets, = the worktree bytes);
`stdlib_table_is_complete` moves 21 → 22 → 27 → 29 in the same
steps; `diag13_ffi_unlinked.stderr` gains the Stage-3 symbols in
the Stage-3 commit and the env symbols in Stage-4. The union of
the four commits is byte-identical to the pre-commit worktree
(plus this report section, which touches no code).

### Follow-ups (dated 2026-11-23; nothing disappears silently)

F1 (CLOSED by the fix-forward below — was NEW/load-bearing/OPEN
in Stage 5; diagnosed with pool introspection, fixed with 5 s TLS
bounds + role-split lanes, re-proved with concurrent load green
locally and in CI): concurrent TLS + blocking-pool parking no
longer deadlocks. Carried from Stage 0 unchanged: (a) E0216
race root-cause, (b) intermittent CI stall, (c) SIGBUS per-program
log-text confirmation, (d) TOML empty-table reporting — plus the
new Wave-1-class item (e2) unquoted YAML scalar ending in `:`
re-parses as a mapping head (pinned by `test_yaml.gzv:75`);
(e) native AOT net/crypto/gzip by design; (f) Windows
crypto/compression (POSIX-compat decision); (g) all-interfaces
bind + Group C; (h) 8.6 KiB wording (cosmetic, closed in
merge-hygiene). Plus the Wave 2 named follow-ups carried in
Stage 0. `toto dev` single-file-only stays a known open
limitation. MySQL `db_exec` empty-misuse-error wart carried above.

### Wave verdict (merged to main; CI green on the re-proof run 45156987879 — supersedes the 45116580699 citation below, kept verbatim)

Criteria 1–7 MET: local full gate (CHECK_EXIT:0, totals above)
plus CI run 45116580699 (Status Success — see CI-proof note
below) covering criterion 6's CI leg. No new builtins, no
D5/type-system change, no checker change, no D1–D8 change, no
Waves 4–5 work, no Windows Tier-1 work, no real credentials
anywhere (fixtures use placeholders; the dev root token is
env-only).

### CI-proof note (dated 2026-11-23; run 45116580699, Status Success)

check green (fmt + clippy + full gate incl. the OIDC SSO suites
against the provisioned Keycloak, LDAP interop, Vault-backed
secrets, and the stdlib corpus); release-gates green; bench green
(incl. the new `exec-refsvc-load` row); Tier-1 macOS + Linux ARM64
probes green (build + conformance + corpus + concurrent program);
Windows probe fails as expected-informational under
`continue-on-error` (POSIX-compat deferred — the ONLY failing
job, the run stays Success). Five fix-forward commits stand on
top of the five stage commits (Keycloak `start-dev` boot, `UID`
→ `KC_UID`, openssl-server retry on output, OIDC client secret
in the create body, scratch fixtures) — the CI-proof note as
first written said "four" and omitted 9cadb58 (OIDC client
secret); corrected here, nothing else reworded. Each diagnosed
from its run's log with local reproduction or direct API
probing, none a guess. Tip run 45119119477 (head 6676490,
Status Success 2026-11-23/24: check + release-gates + bench +
macOS/Linux ARM64 probes green; Windows probe the only
failure, informational) is recorded here — the report previously
recorded only 45116580699 (head 3ae9522). The Stage-5 record's SSO
"[locally] and in CI" bracket is lifted here (proven in this
run's check job) — the bracketed text stands unreworded above.
Wave verdict (updated by the F1 re-proof below, which closes
the F1 load-bearing flag): ACCEPTED pending the user's review
of the flagged auth API surface (criterion 4) and the remaining
follow-ups (burst-ordering limit, recv blind spot, 1-thread
pool, YAML `:` item, MySQL empty-misuse wart — carried by name
in the F1 addendum; E0216, CI stall, SIGBUS log-text, TOML,
native AOT, Windows, Group C as before).

### F1 fix-forward dated addendum (appended 2026-11-23/24; branch tip — report fill follows CI green)

F1 (concurrent TLS + blocking-pool deadlock) reproduced, root-caused,
fixed, and re-proven below. No frozen decision changed (D1–D8, 27
builtins, module model, ADR-009, spec §17/§18 untouched); no new
builtin; no framework API redesign; the two accepted v1 deviations
stay as written.

§1 Step 1 — repro (branch tip 6676490, local Linux x86_64, debug
interpreter; scratch programs in /tmp, never committed): two
concurrent `http_do` GETs against `http_serve_tls` with a
file-blocking handler HANG — serving marker present, zero client
completions in 240 s (normal completions take ~1 s). Three
exclusions all complete: sequential TLS + blocking handler;
concurrent TLS + non-blocking handler; concurrent plain-HTTP +
blocking handler. Shape confirmed exactly as reported.

§1 Step 2 — mechanism (proved, hypothesis corrected). Temporary
pool introspection (submit/pickup/completion logging plus a
one-shot stuck-state dump, all removed before reporting) showed
the stuck pair directly: server-side `TlsClose` blocked in
`SSL_shutdown` waiting for the client's `close_notify` while the
client's own close sat queued behind it (circular wait), with a
client `TlsRecv` stuck on the queued server response beside it.
A 4-concurrent variant then proved a second member of the same
class: client handshakes stuck waiting for server flights whose
server-handshake jobs queued behind them (burst clients always
submit first — connect precedes accept — so the first picks can
all be unblockable). Root cause, revised from the report's
suspicion: ANY pool job that blocks on a peer action can wedge
when the job producing that action is queued behind it on the
fixed 2-thread FIFO pool; the report's "client/server TLS jobs
pile on the same side" was one instance (the close leg). E0216
blind spot confirmed by code reading: the detector requires a
drained pool (`jobs` empty AND `in_flight == 0`) before it may
claim deadlock, so busy-forever pool threads read as future
work; a live listener registration (the accept loop) blinds it
further. A 1-thread-pool experiment additionally showed
handshake waits wedge the same way (kept as a documented limit:
the pool size is not user-configurable; no supported config is
affected). Pool introspection was diagnostic-only and is fully
removed.

§2 Fix (three parts, smallest change that addresses the diagnosed
cause; all in `crates/toto-compiler/src/tlslib.rs`,
`crates/toto-runtime/src/sched.rs`, plus harness hygiene):
(1) Bounded peer waits: `shutdown_close` and the handshake drive
(`ssl_handshake`) now run on a nonblocking fd with a 5 s overall
deadline — a cooperating peer still gets the orderly exchange,
an absent/queued peer yields a loud failure instead of an
indefinite wedge (same 5 s scale as the blocking-I/O default
checkout timeout). `recv`/`send` are deliberately unbounded:
sends complete via the kernel, and bounding recvs would fail
legitimately slow reads (never observed stuck; documented risk,
see follow-ups). (2) Role-split lanes: server-role TLS jobs
(`TlsAccept` by construction; `Send`/`Recv`/`Close` by live
session role via a new `session_is_client` lookup) run on a
dedicated one-thread lane; everything else shares the existing
2-thread pool. The invariant: no lane ever waits on its own
lane's queued work for the gated shapes (client handshakes all
fit the shared lane at the gated concurrency — see below).
(3) Close-on-every-path in the load harness (`load.gzv`
`run_phases` split): an early `?` return used to skip
`net_close(listener)`, stranding the accept loop parked on the
live listener — a silent hang with E0216 blinded (main's error
value is a clean completion, not a fail-fast).
Regression tests: `tls_peer_wait_bounds.rs` (3 tests — silent
peer vs shutdown, server handshake, client handshake; each
returns in ~5 s post-fix and hangs forever pre-fix, verified
both ways).

Second fix in this turn (gate-blocking, pre-existing Wave-3
pool bug, unrelated to F1's mechanism): `proj23_db_concurrent`
flaked (`sqlite prepare: no such table: hammer`) once locally,
once on ARM CI, once on macOS CI. Root cause: `:memory:`
SQLite databases live inside one connection, so the pool pins
them to max 1 — but a later `db_pool_config(dsn, 2, ...)` silently
raised the max, and the moment two checkouts overlapped the
second got a fresh empty handle. Fix: `db_pool_config` clamps
the max to 1 for `sqlite::memory:` (the pin holds against
reconfiguration; 20/20 `HAMMER-OK` post-fix). No test, API, or
threshold changed; `db_pool` 3/3 still green.

Re-proof: the 2-client minimal repro completes (15/15 runs,
`F1-CASE-TLS-BLOCKING-OK`); concurrent Phase C (60 requests,
conc=2 — the proven deterministic envelope, see below) is
green: `phase-c ok=60/60`, `LOAD mode=concurrent conc=2
reqs=60 elapsed_ms=59835 rps_x1000=1002 p50_ms=1989
p99_ms=2089 latn=60`, wall 65 s (methodology: single-connection
sequential baseline was p50 ~1015 ms; 2-way concurrency shares
the interpreted runtime + single server lane — deployment data,
never pinned; bench baseline updated 66496 → 65000).
Deterministic envelope (proven by construction + 5/5 load runs
+ 12/12 repro runs): concurrency is deterministic exactly while
concurrent client handshakes fit the shared lane (2 threads) —
every server handshake then finds its peer running. Above that
the burst submit order can still strand pairs: loud 5 s
handshake timeouts, never a silent hang. Concurrency beyond the
envelope (and the 1-thread-pool handshake wedge) are carried as
named follow-ups with paths (async TLS state machine on the
poller, or elastic pool — both explicitly out of scope here).

§3 report corrections in this addendum: (a) commit count fixed
to five fix-forwards (names above); (b) tip run 45119119477
recorded above; (c) header updated to wave state; (d) this
record; (e) checklist: criterion 4 stays flagged for acceptance
review (untouched); criterion 6 MET on the concurrent numbers
below + local gate (CI leg pending the re-proof run id, filled
next); nothing else changes mark; (f) totals re-derived below
(the fix adds the `tls_peer_wait_bounds` suite).

Fresh full-gate totals (local detached run 2026-11-23,
`CHECK_EXIT:0`; harness `/tmp/checkf1d.sh`, same env/services as
the Wave-3 gate; re-run quiet after the forgery fix with
identical totals): `cargo fmt` clean; `cargo clippy
--workspace --all-targets -- -D warnings` clean;
`cargo test --workspace` green: 74 result lines, 269 passed,
0 failed — bridge from Wave 3 (73 / 266 / 0, derived 2026-11-23):
delta +1 line / +3 tests, exactly the new
`tls_peer_wait_bounds` suite (shutdown + server-handshake +
client-handshake bound, 3/3). Per-binary changes vs Wave 3 are
that suite alone; every other binary holds its Wave-3 count.
Corpus: stdlib `toto test` 24/24; wordfreq 1/1 + interp==built
identity; census 186 (160 top-level + 26 project mains —
unchanged: no corpus case added or removed, `main.gzv` edited
in place for the forgery fix below, goldens byte-identical).
Corpus-flake postscript (pre-existing test bug, found and fixed
this turn — it blocked one local gate attempt and one ARM CI
probe with `forged=BAD` vs `forged=aborted`, single line):
the test forged tokens by flipping the LAST signature character
(`A`<->`B`), but base64url's last char of a 32-byte signature
carries only 4 significant bits — flips within the same high
nibble (e.g. `A`<->`B` differ only in pad bits) decode to
IDENTICAL signature bytes, so the "forgery" verified ~1/16 of
the time (rate matches: 1/20 mine, 1/20 pristine-tip,
1 ARM CI run; decoded bytes proven identical on a caught flip).
The fix flips the FIRST signature character instead (fully
significant; `A`<->`Q` differ in bit 4) — 20/20 `aborted`
post-fix. Crypto, framework, and gate are exonerated: no
product-code change was needed, only the test's tamper method.

§4 Gates + CI re-proof: local gate green as above (74/269/0,
stdlib 24/24, census 186). CI run 45156987879 (head 3d1d6ed,
Status Success 2026-11-23/24): check + release-gates + bench
(incl. the concurrent `exec-refsvc-load` row vs the 65000
baseline) + macOS/Linux ARM64 Tier-1 probes green; Windows
probe fails as expected-informational under `continue-on-error`
(POSIX-compat deferred — the ONLY failing job). Two earlier
re-proof runs failed dissectively first (ARM `proj23` flake →
memory-pin fix above; macOS `proj23` flake → same fix; forged
flakes → forgery fix above), then green. STOP after the report —
no merge.

### Merge-hygiene dated notes (2026-11-23, on main; append-only)

(a) The re-proof run is recorded: run 45156987879 (Status
Success on head 3d1d6ed; check, release-gates, bench, macOS
probe, Linux ARM64 probe all green; Windows probe fails exit
101 as informational, the ONLY failing job). Criterion 6 is MET
on the concurrent numbers (Phase C ok=60/60 at conc=2, p50 1989
ms / p99 2089 ms) + this run. Tip run 45161189944 (head
d01bb10, report-only fill): Status Success, verified
2026-11-23 — recorded here.

(b) Stale-text corrections (append-only; the older text stands
verbatim above). The report header (top lines) read "Status:
WAVE COMPLETE ON THE BRANCH ... F1 fix-forward in progress
(this turn); CI re-proof pending push" — updated to the merged
state at the top of this file. The Follow-ups section listed F1
as "(NEW, load-bearing, OPEN ... concurrent service load stays
deferred until it is diagnosed with pool introspection)" —
F1 is CLOSED by the fix-forward (diagnosed, fixed with 5 s TLS
bounds + role-split lanes, re-proved with concurrent load green
locally and in CI); the entry now says so, and the old OPEN text
no longer stands as the current state. The verdict heading cited
run 45116580699 — corrected to the re-proof run 45156987879.
Recorded here: §3(c)'s "header updated" claim was false as
written (the header still carried the branch-state wording, and
the follow-ups entry still read OPEN); the correction lands in
this hygiene commit, not earlier.

(c) ARM/macOS attribution reconciliation. §4 says "ARM `proj23`
flake → memory-pin fix", but the corpus-flake postscript and the
6b5e663 commit message attribute the single ARM corpus-gate
failure in run 101 (45141642332) to the forgery bug, and the
sequence supports forgery=ARM (ARM green in run 102,
45152491170, after the forgery fix) and proj23=macOS (macOS green
in run 103, 45156987879, after the :memory: fix). Reconciled and
recorded here: the ARM failure in run 101 is attributed to the
forged-token flake (`forged=BAD`, log-observed); the macOS
failure in run 102 is attributed to `proj23` (`sqlite prepare:
no such table: hammer`, log-observed); §4's "ARM proj23" line
is corrected as a misattribution. The CI logs fetched via the
API show exactly these two signatures, and they are the
authority here.

(d) Addendum date correction: "2026-11-23/24" is wrong — runs
45156987879 and 45119119477 are both 2026-11-23 in UTC, Manila,
and PST. The F1 addendum's date reads 2026-11-23.

(e) Branch total: 15 commits (5 stage: 4026697, 3539c58, a7c494d,
53f0670, 1d9db1b; 5 fix-forwards: 57bbe91, afa8a7d, 59bd348,
9cadb58, 3ae9522; 1 Wave-3 fill: 6676490; 4 F1 fix-forward
commits: 0e79693, 6b5e663, 3d1d6ed, d01bb10). §3(a) corrected
four→five fix-forwards and named 9cadb58, which stands.

(f) User-accepted flags, recorded as written 2026-11-23: the
auth API surface (criterion 4) — exactly the five stdlib modules
(gzv.std/password, gzv.std/jwt, gzv.std/oidc, gzv.std/saml,
gzv.std/ldap) plus the OIDC discovery/auth-url/exchange/verify/
userinfo shapes in the Stage 3 design section; the conc=2
deterministic envelope for concurrent TLS service load (beyond
conc=2, burst submit order can strand pairs with loud 5 s
handshake timeouts, never a silent hang — the async TLS state
machine on the poller, or an elastic pool, is the named
follow-up and is NOT this merge); SAML without XML-DSig/RSA
signature verification (surface + structural parsing +
conditions validation only, never claimed as validated auth);
cloud secret stores (AWS/GCP/Azure) seam-only (Vault is the only
live secret store). Remaining carried follow-ups by name: recv/
send unbounded peer waits (documented risk), 1-thread pool
wedge (documented config limit), E0216 root-cause (Phase 4
follow-up — the guard still cannot see a busy-forever pool),
intermittent CI stall, SIGBUS per-program log-text confirmation,
TOML empty-table reporting, YAML `:` item, MySQL empty-misuse
wart, native AOT net/crypto/gzip, Windows DEFERRED WITH
EVIDENCE, Group C items.

(g) Test totals confirmed: 74 result lines / 269 passed /
0 failed (73+1 / 266+3, exactly the `tls_peer_wait_bounds`
suite); stdlib 24/24; corpus census 186.
