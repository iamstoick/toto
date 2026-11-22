# Enterprise Wave 3 report — data and identity

Status: STAGE 0 COMPLETE (starting-state verification + carry-forward
disposition, this turn). Earlier section text is preserved verbatim;
the report is appended stage by stage.

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
   (Stage 0 above).
2. Database drivers + pooling — NOT YET (Stage 1).
3. Migrations — NOT YET (Stage 2).
4. Auth — NOT YET (Stage 3).
5. Config + secrets — NOT YET (Stage 4).
6. Reference service + load proof + SSO demo + gates — NOT YET
   (Stage 5).
7. Report complete — NOT YET (appended per stage).

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
