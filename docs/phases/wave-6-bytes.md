# Wave 6 — bytes/binary story (Group C item 23)

Decision record (2026-11-26; architect-owned on explicit
user delegation to finish the project). Group C item 23 —
"a designed way to hold, slice, read, and write binary data,
plus hex/base64 helpers" — is the keystone the roadmap's own
build order puts first ("packet code has nothing sound to
stand on without it"). This wave delivers it.

## Spike verdict (recorded here; probed live this turn)

- Carrier: `Array<int>` (byte values), already established by
  `jwt_b64_encode/decode`, `pw_hex_encode/decode`, crypto and
  saml helpers. No new builtin type, NO spec/checker/runtime
  change.
- Slicing: implementable (`slice` via loop + `push`, verified
  live: `[10,20,30,40][1..3] == [20,30]`); `push` mutates,
  `a[i]` reads, `a[i] = v` writes, `len` measures.
- Pack/unpack without shifts: the language has no `<<`/`>>`
  or bitwise ops, but `u16le = b[0] + b[1]*256`, `hi = n/256`,
  `lo = n%256` — all verified live (`0x1234` → 18, 52).
- The gap is real: 4+ duplicated b64/hex table copies
  (jwt, saml, password, trace, http), no canonical home.

## Scope (stdlib-only)

New `stdlib/bytes.gzv` (`//!` + `///` from birth): shared
b64/b64url/hex tables; `bytes_b64_encode/decode`,
`bytes_b64url_encode/decode`, `bytes_hex_encode/decode`
(`Array<int>` <-> `string`, decode errors as `Result`);
`bytes_slice`, `bytes_concat`, `bytes_is_valid`;
`bytes_u16le/u32le/u16be/u32be` getters plus `_bytes`
encoders. Panic-on-OOB (matches `data[i]` semantics),
documented. New `stdlib/tests/test_bytes.gzv` with RFC 4648
vectors, hex round-trips, LE/BE edge vectors (0, max,
0x1234), slice edges, invalid-decode `Err` paths.

Phase A (this wave, first commit): module + tests, green,
standalone — zero risk to existing modules. Phase B (second
commit): migrate the duplicated copies to thin wrappers over
`bytes_*` (jwt, saml, password, trace, http hex paths),
behavior-identical, proven by the 25/25 stdlib suite +
corpus. New files follow house stdlib style (`pub` +
4-space, like the migrated tree; stdlib is not fmt-gated).

## Acceptance

- `test_bytes.gzv` green in the stdlib suite; full suite
  stays 25+x/25+x (x = new file) with zero behavior drift
  in Phase B (item-name + golden equivalence).
- `toto doc` renders the new module (check.sh doc gate
  covers it via the `stdlib/*.gzv` glob + pub-name check).
- `toto lint --deny-warnings` clean over the repo (gate).
- `cargo fmt --check` + `cargo clippy` clean locally (the
  new oracle), CI green on both commits.

## Non-goals and known limits (recorded, not deferred silently)

- No new type, no shifts/masks, no spec §/checker/runtime
  change. Native byte buffers (perf) stay a future gated
  proposal IF measurement under generated code demands it.
- No other Group C item starts here (raw sockets need a
  privilege story unverifiable in this sandbox; extra
  protocols need product scope; toolkit needs the substrate).
  No registry work (separate deferral note).
- No fmt normalization of stdlib (pre-existing pub-drop).

## What this does NOT do

Does NOT write code in this turn (implementation follows
immediately in-wave). Does NOT change D1–D8, ADR, module
model, spec, the 27-builtin catalog, or any existing stdlib
API surface (wrappers keep names + semantics).

## Phase A record (2026-11-26; CI run 45817063343 pending at
Phase B commit time — see below)

Phase A CI outcome (run 45817063343, recorded here): check
FAILED — missed count pin. My Phase A verification ran the
stdlib suite, both gates, fmt/clippy, and the front suites
but NOT `cargo test --test project`; the new module made
`MODULES.len()` 31 against the test's hardcoded 30. Caught
by CI instead of locally — process gap, owned. The fix
(count 30→31) is already in the Phase B commit, verified
locally 15/15; confirmation awaits the failed-step log
(bench still running when observed) plus the Phase B run.

Phase B CI outcome (run 45819168935, recorded here):
concluded SUCCESS — check + release-gates + bench +
macOS/Linux-ARM green (Windows: the known informational
failure). This green covers both phases' code (Phase B tree
is a superset of Phase A), closing the count-pin miss above:
the full gate, including `cargo test --workspace` with the
31-module table, passes on the final tree. Wave 6 COMPLETE.

Phase B record (2026-11-26; CI green — see above): dedup wrappers as
designed — `jwt_b64_*` → `bytes_b64url_*`, `saml_b64_decode`
→ `bytes_b64_decode`, `pw_hex_*` → `bytes_hex_*`,
`trace_hex` → `bytes_hex_encode`; dead private tables
removed (verified zero remaining references, tests use only
public names). One judgment recorded here: `pw_hex_decode`
widens to both-case input (canonical rule; no in-tree
uppercase producers/consumers/tests, documented on the
wrapper). `http_hex_val` and `trace_hexval` untouched
(different shapes: pct-decoder internals, validation
predicates). `trace.gzv` E0502 (`kind_ok`) is a pre-existing
documented gate exclusion — left alone, not introduced here.
Verification: stdlib suite 26/26 (incl. RFC/JWT/pbkdf2
vectors through the wrappers), lint gate clean (exclusions
unchanged), doc gate clean, fmt clean, clippy clean under
`-D warnings` except the two pre-existing netsys lints CI
already accepts, compiler front + CLI + corpus suites green,
wordfreq green. The `stdlib_table_is_complete` count pin
30→31 (new module registered).

Built as designed, no deviations: `stdlib/bytes.gzv`
(17 pub + 5 private fns, `//!` + `///` from birth),
`stdlib/tests/test_bytes.gzv` (RFC 4648 §10 vectors,
folded-whitespace, loud-error paths, LE/BE edges incl. 0/max,
round-trips, range rejections), plus the one-line
`std_module!("bytes")` registration. Two fixes during
verification, both recorded here: (1) tail-position `if`
needs its `else` (E0107; house empty-else used);
(2) `== []` needs a typed binding (E0309). Verified: stdlib
suite 26/26, doc gate clean (22 headings incl. the new
module), lint gate clean, fmt + clippy clean locally (only
the two pre-existing netsys warnings CI already accepts),
compiler front suites + CLI lint/coverage/doc/corpus +
wordfreq green. Commit triggers CI; outcome in a future note.
