# `gzv.std` — standard library (Phase 5, Stage 2)

Nine modules of ordinary Toto on the frozen 27 builtins (plus the
FFI modules, which call real C). Import by canonical path; the
sources in `stdlib/*.gzv` are embedded in every `toto` binary (see
[project.md](project.md)).

```gzv
import "gzv.std/strings.gzv";
import "gzv.std/json.gzv";

fn main() -> int {
    println(pad_left("42", 5, "0"));
    println(json_stringify(json_int(42)));
    return 0;
}
```

Conventions across all modules:

- No new builtins were added: the frozen catalog stands. Character
  access goes through `split(s, "")` (the builtin splits into Unicode
  scalar values); string building is `+` concatenation.
- No user generics exist in v1, so element-wise helpers come in
  per-type variants (`map_ints`, `map_strings`, …). `int` and
  `string` are covered; the pattern extends on demand.
- Flat namespace: every public *and private* stdlib item shares one
  program-wide scope with user code. Private helpers carry a
  module prefix (`json_skip_ws`, …); defining a top-level item with
  the same name as any stdlib item is `E0311`. Public names are the
  API below — treat them as reserved.
- Checked arithmetic applies: `pow_int`/`factorial` panic on overflow
  rather than wrap.

## `strings`

Predicates, joining, and slicing on the `split(s, "")` character path.
Indices are character (scalar) positions, agreeing with `len`.

| Function | Behavior |
|---|---|
| `starts_with(s, prefix)` | `split`-based prefix test; empty prefix is true |
| `ends_with(s, suffix)` | mirror of the above |
| `join(parts, sep)` | concatenate with separator; empty array joins to `""` |
| `repeat(s, n)` | `s` concatenated `n` times (`n = 0` gives `""`) |
| `replace(s, from, to)` | all non-overlapping occurrences; empty `from` returns `s` |
| `char_at(s, i)` | one-character string; out-of-range panics `E0209` |
| `substring(s, start, end)` | half-open character range; out-of-range panics `E0209` |
| `index_of(s, needle)` | first character index, or `-1`; empty needle is `0` |
| `pad_left(s, width, pad)` | prepend `pad` until `len` reaches `width` |
| `pad_right(s, width, pad)` | append `pad` until `len` reaches `width` |
| `is_empty(s)` | `len(s) == 0` |

## `collections`

Higher-order array helpers. Functions pass as values (named or
inline closures); all builders return fresh arrays, while `sort_ints`
and the `reverse_*` sort in place (arrays are shared by reference)
and return the same array for chaining.

Ints: `map_ints`, `filter_ints`, `fold_ints`, `sum_ints`,
`any_int`, `all_int` (`all` on `[]` is true), `find_index_int`
(`Option<int>`), `contains_int`, `sort_ints` (insertion sort),
`reverse_ints`.
Strings: `map_strings`, `filter_strings`, `contains_string`,
`reverse_strings`, `find_index_string`.

```gzv
import "gzv.std/collections.gzv";

fn is_big(x: int) -> bool {
    return x > 2;
}

fn main() -> int {
    println(sum_ints(filter_ints([1, 2, 3, 4], is_big)));
    return 0;
}
```

## `math`

Exact integer operations beyond the builtins
(`abs`/`min`/`max`/`sqrt`/`floor`/`ceil` stay builtins):
`clamp_int`, `clamp_float`, `pow_int` (non-negative exponent;
`pow_int(b, 0) == 1`), `gcd`, `lcm` (`lcm(0, x) == 0`), `factorial`,
`is_prime` (trial division; `1` is not prime).

## `time`

Pure durations: a duration is an `int` count of milliseconds, and a
timestamp is an `int` count of milliseconds since an
application-chosen epoch — plus the Wave 2 monotonic clock
(Stage 3). **There is still no wall clock**: the frozen builtins
expose no clock, so the clock arrives as a linked foreign function
(`toto_rt_clock_monotonic_ms`, unix-only), not a builtin — the
catalog stays frozen. The clock is monotonic ms since process
start (ADR-009): absolute values compose across fan-out.
Values are nondeterministic by construction (time passes between
scheduling points), so they are never pinned in goldens; compare
differences or inequalities only.

Constructors `duration_from_millis/secs/mins/hours`, projection
`duration_as_millis/secs` (truncating), `duration_add/sub`,
three-way `duration_compare` (`-1`/`0`/`1`), `format_duration`
(`"500ms"`, `"90s"`, `"2m 5s"`, `"3h 4m"`, `"2d 1h"`, `"-"` prefix
for negatives), `timestamp_add`/`timestamp_diff` intent markers,
and the clock: `clock_monotonic_ms()` plus
`clock_elapsed_since(t0)` (non-negative on both engines).

## `io`

`read_lines(path)` splits `read_file` on `"\n"`, dropping one
trailing empty (a trailing newline terminates the last line; it is
not an extra line — mirrors `str::lines`; interior empties are kept).
`write_lines(path, lines)` joins with `"\n"` and overwrites.
`append_line` is read-modify-write and **not atomic**: concurrent
appends can lose lines; serialize appends through a channel instead.
`print_lines` prints each element.

Tests seed work files by overwrite first: there is no delete
builtin, so the suite never relies on a file being absent.

## `fs`

Pure path surgery (`path_join`, `basename`, `extension`, `stem`,
`is_absolute` — `/`-separated, no normalization); file
reads/writes stay the builtins. `stem("archive.tar.gz")` is
`"archive.tar"`.

## `json`

Parse (`json_parse`) and serialize (`json_stringify`) with typed
accessors over a `JsonVal` struct:

```gzv
struct JsonVal {
    kind: string,      // "null" | "bool" | "num" | "str" | "arr" | "obj"
    raw: string,       // bool/number literal, or unescaped string content
    items: Array<string>, // arr elements as compact JSON
    keys: Array<string>,  // obj keys
    vals: Array<string>   // obj values as compact JSON
}
```

Nested values store as **compact JSON strings**, parsed on access:
Toto v1 rejects recursive types (`E0302`), so a directly nested DOM
is inexpressible and this strata design is the honest encoding.
`json_array_get`/`json_object_get` re-parse the nested slice on every
access (`O(depth)` per descent, no caching).

Constructors: `json_null/bool/int/float/string`,
`json_array(elems: Array<JsonVal>)`,
`json_object(keys, vals)` (`Result`: `Err` on length mismatch).
Accessors return `Result` with plain messages:
`json_kind/as_bool/as_int/as_float/as_string`,
`json_array_len/get` (`Err` on wrong kind or out-of-bounds),
`json_object_get` (`Err` on wrong kind or missing key).

Recorded limits (all covered by tests in `stdlib/tests/`):

- Numbers keep their raw literal: `json_as_int` rejects non-integers
  and `parse_int` panics `E0210` past i64 range (inherited, not
  wrapped). Floats parse via float arithmetic — exact decimal
  semantics is not guaranteed — and serialize via `to_string`
  (Rust `Debug`, valid JSON).
- `\uXXXX` escapes are rejected with a clear error: no
  codepoint-to-character builtin exists in the frozen catalog.
- Other C0 controls pass through raw on output.
- Depth is bounded by interpreter/JIT stack limits, not by this
  module; keep nesting shallow.
- Error messages are the contract: `t_errors` pins each one
  (`"unterminated string"`, `"trailing characters after JSON
  value"`, `"unicode escapes (\\u) are not supported"`, …).

## `ffi`

C ABI wrappers over real libc/libm/libpthread (see [ffi.md](ffi.md)
for the per-binding ownership/encoding/thread-safety contracts).
Non-callback wrappers build: `apps/wordfreq` (per-line `c_strlen`)
links a standalone binary with byte-identical output. Foreign
threads live in `gzv.std/ffi_thread`, which is run/test-only —
`c_thread_spawn` hands a callback to `pthread_create`, and a called
callback site refuses at compile time (exit 3). Every wrapper's
contract (strings as NUL-copies, `open` failing as `-errno`,
callbacks drained on the joining worker) is pinned by
`test_ffi.gzv` and the `proj11_ffi` corpus project.

## `net`

Networking substrate (Enterprise Wave 1; unix-only — see
[ffi.md](ffi.md) for the per-binding contract). Sockets are opaque
`int` handles; bytes cross as `Array<int>` (elements must be
0–255, `E0308` otherwise — the only foreign array shape in v1).
Every function returns `Result<_, string>` with the OS message
(`strerror` discipline); closing twice or using a closed socket
is an `Err`, never a panic, never silent. `listen`/`bind` are
loopback (`127.0.0.1`) only; there are no timeouts in v1.

| Function | Behavior |
|---|---|
| `net_tcp_connect(host, port)` | Pool-DNS resolve, then poller-parked handshake; `Ok(fd)` |
| `net_tcp_listen(port)` | Loopback bind + listen; `Ok(listener fd)` |
| `net_accept(listener)` | Poller-parked accept; `Ok(conn fd)` |
| `net_send(conn, data)` | One nonblocking syscall; may short-send (`Ok(n)`) |
| `net_send_all(conn, data)` | Toto-level loop to full delivery; `Ok(len(data))` |
| `net_recv(conn, max)` | Up to `max` bytes; empty array is orderly shutdown, not an error |
| `net_close(sock)` | Close + unregister; `Ok(0)` |
| `net_resolve(host)` | Blocking-pool DNS; `Ok(Array<string>)` (comma-joined inside) |
| `net_udp_bind(port)` | Loopback datagram socket; `Ok(fd)` |
| `net_udp_send_to(sock, data, host, port)` | Resolve + send; `Ok(n)` |
| `net_udp_recv_from(sock, max)` | `Ok(UdpPacket)` (`{ data, host, port }` struct) |
| `last_error()` | Read the per-task call-duration error slot; always read immediately |

Network waits park the task on the M:N poller (a worker never
blocks on a socket); DNS parks on the dedicated blocking pool.
The JIT/AOT task runtime has no poller integration: net calls
refuse at compile time (exit 3, "needs the M:N poller") — run
net programs with `toto run`/`toto test`. Pinned by the
`proj11_net_echo` corpus project (loopback TCP echo + UDP echo).

## `crypto`

Cryptographic bindings (Enterprise Wave 1; unix-only — see
[ffi.md](ffi.md) for the per-binding contract and
[spec §18](spec.md) for the API record). Primitives are
bound (system libcrypto), never implemented. Bytes cross as
`Array<int>` (0–255, `E0308` otherwise). Every fallible
function returns `Result<_, string>`; malformed inputs are
runtime `Err` strings, never panics. Layouts: digests 32/64
bytes; AES-256-GCM output is `ciphertext || tag(16)`;
`SignKeyPair { secret(32), public(64, x||y) }`, DER signatures
(ECDSA P-256 over SHA-256); `verify`/`x509_verify` return 1/0
with a clean slot, -1 plus the slot on malformed input.

| Function | Behavior |
|---|---|
| `crypto_sha256(data)` / `crypto_sha512(data)` | Digests; `Ok(bytes)` |
| `crypto_hmac_sha256(key, data)` / `crypto_hmac_sha512(key, data)` | Tags; `Ok(bytes)` |
| `crypto_aead_encrypt(key, nonce, pt, aad)` | AES-256-GCM; `Ok(ct \|\| tag)` |
| `crypto_aead_decrypt(key, nonce, cttag, aad)` | Authenticates or `Err` |
| `crypto_sign_keygen()` | Fresh P-256 keypair; `Ok(SignKeyPair)` |
| `crypto_sign_sign(secret, msg)` | DER signature; `Ok(bytes)` |
| `crypto_sign_verify(public, msg, sig)` | `Ok(1/0)`; `Err` on bad input |
| `crypto_random(n)` | `RAND_bytes`, 1–65536; `Ok(bytes)` |
| `crypto_x509_verify(leaf_pem, ca_pem)` | `Ok(1)` iff the chain verifies |
| `crypto_openssl_version()` | Runtime library version (never pinned) |

Calls run inline on the worker (one-shot OpenSSL calls — no
parking, no pool). JIT/AOT refuse like the net surface (run
with `toto run`/`toto test`). Pinned by `proj13_crypto_kat`
(published/pinned vectors per primitive) and
`proj14_crypto_errors` (every malformed input is `Err`).

## `tls`

TLS upgrades for loopback TCP (Enterprise Wave 2, Stage 1;
unix-only — see [ffi.md](ffi.md) for the per-binding
contract). The state machine runs in the linked system libssl
(OpenSSL 3 on Linux, LibreSSL on macOS) — bound, never
implemented. Connections keep their `int` handles: TLS upgrades
an already-connected TCP fd in place (client: `net_tcp_connect`
then `tls_client_connect`; server: `net_tcp_listen`/`net_accept`
then `tls_server_accept`), so the Wave 1 loopback rule carries
over unchanged and no new socket layer exists. Bytes cross as
`Array<int>` (0–255, `E0308` otherwise); every function returns
`Result<_, string>`, closing twice or using a non-TLS socket is
an `Err`. Trust model (Wave 1 reuse): the client verifies the
server chain against the given CA PEM with the same
`X509_STORE` semantics as `crypto_x509_verify`, enforced at
handshake time — no hostname check (as in Wave 1), no client
certificates, one-way loopback TLS only.

| Function | Behavior |
|---|---|
| `tls_client_connect(conn, ca_pem)` | Upgrade a connected fd, verifying the chain; `Ok(fd)` |
| `tls_server_accept(conn, cert_pem, key_pem)` | Upgrade an accepted fd; `Ok(fd)` |
| `tls_send(conn, data)` | One record write; may short-write (`Ok(n)`) |
| `tls_send_all(conn, data)` | Toto-level loop to full delivery; `Ok(len(data))` |
| `tls_recv(conn, max)` | Up to `max` bytes; empty array is orderly shutdown, not an error |
| `tls_close(sock)` | `close_notify` + free + close; `Ok(0)` |

Handshake/record steps are blocking calls (interpreter) or
blocking-pool waits (executor) — never the poller, which serves
only `poll()`able readiness waits (spec §17.5 rule: an SSL step
is not one). JIT/AOT refuse like the net surface (run with
`toto run`/`toto test`). Pinned by `proj15_tls_echo` (Toto
loopback echo through the real wrappers) and the
`tls_interop`/`tls_openssl` suites (both directions against
`openssl s_server`/`s_client`, plus the untrusted-chain
negative).

## `http`

HTTP/1.1 framework (Enterprise Wave 2, Stage 2; unix-only via
the net/tls surface — this module itself is pure Toto, no new
FFI). Routing (exact > template > wildcard precedence,
first-registration tiebreak, `"*"` methods), onion middleware,
and handlers run on green threads over the Wave 1 substrate:
each accepted connection gets its own task, waits park on the
M:N poller, TLS steps on the blocking pool. The client runs over
the same substrate, upgrading to TLS for `https://`. HTTP/1.1
only (keep-alive, `Content-Length`, chunked request/response
bodies); no new builtins — the ASCII framing codec is pure Toto.

| Function | Behavior |
|---|---|
| `http_router()` | Empty router (no routes, no middleware) |
| `route_add(router, method, pattern, handler)` | Register; `Ok(())`, or `Err` on empty method / bad pattern |
| `use_mw(router, mw)` | Append onion middleware (registration order) |
| `http_serve(port, router, opts)` | Loopback listen + accept loop; `Ok(listener fd)` |
| `http_serve_tls(port, router, opts, cert, key)` | Same, with a server TLS upgrade per connection |
| `http_do(req, opts)` | One exchange + redirects; `Ok(HttpClientResp)` |
| `http_get(url, opts)` / `http_post(url, headers, body, opts)` | One-shot shorthands (`https` needs `ca_pem`, use `http_do`) |
| `http_header_get(headers, name)` | Case-insensitive; `Err("http: missing header …")` when absent |
| `http_header_or(headers, name, dflt)` | Same, with a default (always `Ok`) |
| `http_param(params, name)` | Path-param lookup; `Err("http: missing param …")` when absent |
| `http_decode_pct(s)` / `http_parse_query(target)` | Percent-decoding / query pairs; `Err` on malformed input |
| `http_bytes_to_text(data)` / `http_text_to_bytes(s)` | Framing-text codec; fixed `Err` on unrepresentable bytes |
| `http_response(status, reason, headers, body)` / `http_text(status, reason, text)` | Response constructors |

Fixed wire mapping (golden-pinned): handler/middleware `Ok`
serializes as-is; their `Err` closes the connection with no
response (a v1 language limit — `Err` payloads are not values,
so the 500 mapping needs Phase-2 destructuring or a gated new
builtin; `expose_errors` is reserved and inert); unknown route
→ 404; known path with a wrong method → 405 + `Allow`;
bad framing/size accounting → 400; other-than-1.x version → 505.
Oversize/truncated requests map to 400 (handlers never see a
partial body); a vanished client closes quietly. Redirects:
`redirects: 0` never follows; otherwise same-scheme/host/port
(or relative) targets only, method rules 301/302/303/307/308
fixed, budget overrun is `Err("http: too many redirects")`.
`host_allow` narrows the loopback set (`localhost`,
`127.0.0.1`, `::1`); everything else is rejected before
connecting. `keepalive_ms` is advisory (no clock in v1);
`peer` is the loopback placeholder until the substrate exposes
TCP peer addresses.

Pinned by `test_http.gzv` (pure units), `proj16_http` (full
lifecycle: routing, middleware ordering + short-circuit, error
paths, close semantics, TLS both directions), and the
`http_interop` suite (our server vs curl plain + https with
`--cacert`; our client vs a curl-checked reference and vs
`openssl s_server`).

## `compress`

gzip compression (Enterprise Wave 1; unix-gated like the other
surfaces — the backend itself is portable pure Rust, so lifting
is cfg-only). The codec is `flate2`/`miniz_oxide` (no C code).
`gzip_compress(data)` returns the RFC 1952 stream (bytes never
pinned — headers may vary); `gzip_decompress(data)` returns the
payload or `Err("decompress: invalid gzip data")` (one fixed
string, never library wording). Pinned by `test_compress.gzv`
(round-trips plus a `gzip -n` oracle vector; the encoder is
cross-checked against system gunzip — see the Wave 1 report).

## `toml`

TOML parse + serialize, config-shaped subset (Enterprise Wave
1; pure Toto code — see [spec §19](spec.md) for the scope
record). Values are `TomlVal` structs mirroring the `JsonVal`
strata design (nested tables store as source-line fragments,
re-parsed on access). Tables, dotted tables/keys, strings,
ints (with `_`/signs), bools, floats, single-line scalar
arrays, `#` comments. NOT covered (clear errors): inline
tables, arrays of tables, datetimes, multi-line strings,
hex/octal/binary ints, `\u` escapes. `toml_parse` /
`toml_stringify` round-trip (modulo whitespace/comments);
`toml_table_get` / `toml_table_get_array` / `toml_array_get` /
`toml_as_*` accessors mirror the json ones. Pinned by
`test_toml.gzv` (shapes, reparse stability, error paths).

## `yaml`

YAML parse + serialize, config-shaped subset (Enterprise Wave
1; pure Toto code). Values are `YamlVal` structs (same strata
design; nested blocks store dedented, re-parsed — and
validated — on access). Block mappings/sequences, nesting by
spaces-only indentation, plain/single/double-quoted scalars,
int/bool/null/float, `#` comments, inline `- k: v`. NOT
covered (clear errors): anchors, tags, flow styles,
multi-document streams, tabs, `\u` escapes. Canonical
2-space-indent `yaml_stringify` (re-parse-stable;
cross-checked with PyYAML). Pinned by `test_yaml.gzv`.

## `proto`

Protobuf wire codec, varint/length-delimited subset
(Enterprise Wave 1; pure Toto code). No `.proto` compiler or
codegen: schema-driven `pb_encode`/`pb_decode` over explicit
`PbField`/`PbSchema` descriptors (field numbers 1–2²⁹−1;
wire types 0 varint non-negative-only, 2 bytes). Unknown
fields are skipped per protobuf rules; unknown wire types,
groups, fixed32/64, and negative ints are clear errors.
Strings cross as bytes (the caller maps text). Pinned by
`test_proto.gzv` (hand-computed golden wire bytes for a
stated `Person` contract, verified against the encoding rules
independently of the implementation).

## `context`

Request-scoped values, deadlines, cancellation (Enterprise Wave 2,
Stage 3; implements ADR-009 exactly). `Context` is an immutable
library value (`pairs` plus an absolute monotonic-ms `deadline`,
`-1` = none): `ctx_with_value` derives, never modifies;
`ctx_get`/`ctx_get_or`/`ctx_has` read; `ctx_with_deadline` stores
`clock_monotonic_ms() + after_ms` (only exactly `-1` means none —
negative absolute times are legitimate past deadlines);
`ctx_no_deadline`/`ctx_deadline` manage the carried value.

The current task's slot is authoritative: `ctx_set_current`
installs a value (children spawned after inherit it —
ADR-009 spawn inheritance, recorded at `Spawn`), `ctx_current`
reads it back, `ctx_expired()` inspects the slot deadline without
parking (both engines). Contexts never ride channels by default
(receiver scope governs); the opt-in edge helper is
`log_trace_edge` (in `gzv.std/log`), and message-scoped transfer
awaits user generics (Wave 4 — not smuggled in).

Cancellation is scope failure: `ctx_with_cancel()` snapshots the
caller's current scope into a `CtxCancel` handle (no scope is
created, no task moves); `ctx_cancel(h)` fails exactly that scope
through the `cancel_scope` sweep — parked tasks release with
E0214, running tasks fail fast at the next suspension point, and
the failure propagates upward like any scope panic (cancellation
is program-fatal by design). Forged handles are `Err` values
(`-1` + slot), never silent. `ctx_detach_next()` arms one spawn
to start empty; `go_detached(f)` wraps the pattern for closures.

Pinned by `test_context.gzv` (values, slot round-trip,
inheritance, one-shot detach, deadline values + inspector,
forged-handle message) and the corpus `proj17_context_log`
(pipeline) / `proj18_ctx_cancel` (fatal golden) pair.

## `log`

Leveled machine-readable structured logging carrying context
values with no per-call-site plumbing (Enterprise Wave 2,
Stage 3). Levels `debug`/`info`/`warn`/`error` plus `off`
(`log_set_level`; the gate runs in the runtime, so filtered calls
cost one int compare and never format). Format: one JSON object
per line in exact field order
`{"ts":..,"level":"..","msg":"..","ctx":{..}}` with sorted ctx
keys; `ts` is Wave 2 monotonic ms (never pinned — golden
pipelines strip or avoid it); `ctx` is the CURRENT task's pairs.
Escaping covers `"`/`\`, `\n \r \t \b \f`, and `\u00xx`
(lowercase) for other controls; lines never contain raw newlines.
Sinks: `stdout` (captured output; one `println` per record, never
interleaved), `stderr` (fd directly, one write per line),
`buffer` (in-memory retention for `log_drain_lines` — tests and
golden pipelines). No file sink in v1 (no append builtin).
`log_debug/info/warn/error` return `Ok(1)` for a produced stdout
line, `Ok(0)` otherwise (filtered or non-stdout sink), `Err`
only on runtime misuse. `log_trace_edge` records the Wave-2
causality edge at `debug` with the sender's context attached.

Pinned by `test_log.gzv` (gate, exact wire format rebuilt around
the observed `ts`, escaping round-trip, empty ctx, edge
record, sink returns — sequential, so it covers the interpreter
arms) and the corpus `proj17_context_log` buffer golden.

## `metrics`

Prometheus metrics (Enterprise Wave 2, Stage 4; pure Toto over
the `http` framework — no new builtins, no new FFI). `MetricsRegistry`
holds `counters`, `gauges`, and `histograms` (registration order);
every definition carries `name`, `help`, and `label_names`, and
each observation adds to the sample under its label-value set
(first-observation order). Counters hold monotonic `int` totals
(`metrics_inc` rejects negative deltas); gauges hold `int` values
(`metrics_gauge_set`/`metrics_gauge_add`, the delta may be
negative); histograms hold cumulative per-bucket `counts` over
`int` bounds plus `sum`/`count`. Values are ints (counts,
millisecond latencies); float observations are a follow-up.

| Function | Behavior |
|---|---|
| `metrics_registry()` | Empty registry |
| `metrics_exposition_version()` / `metrics_content_type()` | `"0.0.4"` / `text/plain; version=0.0.4; charset=utf-8` pins |
| `metrics_register_counter/gauge/histogram(reg, name, help, labels[, buckets])` | Register; `Err` on bad/duplicate name, bad/reserved label, bad buckets |
| `metrics_inc(reg, name, labels, delta)` | Counter add; `delta >= 0`, `Err` otherwise |
| `metrics_gauge_set` / `metrics_gauge_add(reg, name, labels, v)` | Absolute set / signed add |
| `metrics_observe(reg, name, labels, value)` | Histogram observation (cumulative buckets) |
| `metrics_render(reg)` | Full 0.0.4 exposition (`HELP`/`TYPE` + samples, `\n`-terminated) |
| `metrics_respond(reg)` / `metrics_handler(reg)` | 200 response / `GET`-ready handler closing over the registry |

Registration is mandatory and unique across all kinds (a counter
and a gauge may never share a name); names match
`[a-zA-Z_:][a-zA-Z0-9_:]*`, label names add the `__`-prefix and
histogram-`le` reservations. Render order is counters, gauges,
histograms (samples in first-observation order); HELP escapes `\`
and LF, label values escape `\`, `"`, and LF. The scrape endpoint
is `route_add(router, "GET", "/metrics", metrics_handler(reg))`
plus `http_serve` (loopback rule rides along); the handler closure
shares the registry arrays, so scrapes see every observation.

Pinned by `test_metrics.gzv` (version pins, exact renders incl.
escaping and cumulative math, every fixed error, alias isolation,
handler over a synthetic request) and the corpus `proj19_metrics`
golden (error paths + a live serve/scrape byte-match).

## `db`

Database drivers over pooled mature clients (Enterprise Wave 3,
Stage 1). `DbVal` is `{kind:int, i:int, f:float, s:string}`
(`kind`: 0 null, 1 int, 2 float, 3 text, 4 bool); constructors
`db_null/int/float/text/bool` build it. DSN selects the driver:
`sqlite:path=/x/y.db` (file), `sqlite::memory:` (process DB),
`postgres:host=.. port=.. dbname=.. user=.. password=..
[connect_timeout=..]`, `mysql:host=.. port=.. dbname=..
user=.. password=.. [connect_timeout=..]`. `db_exec` runs a
statement (affected rows; DDL counts are driver-defined);
`db_query` returns `Array<Array<DbVal>>`; `db_pool_config`
sets max connections + checkout timeout; `db_close_pool`
retires a pool; every fallible fn returns `Result<_, string>`
with the library message. `?` placeholders bind server-side
(Postgres `?`→`$n`, SQLite native) or through the library
escaper (MySQL) — never interpolated. Value decoding is
native per driver (int/float/bool/text); Postgres numerics
arrive as text (precision kept); MySQL decodes by shape
(integer/float-shaped → int/float, else text — `CAST AS
CHAR` when the distinction matters; no blob in v1).
Non-finite floats on the interpreter path are a loud `Err`
(Toto cannot construct NaN/±Infinity; the runtime engine
handles them natively). Pool: one process-global pool per
(driver, DSN), default max 8 / checkout timeout 5000 ms;
exhaustion is `Err("pool exhausted ...")`, never a stall;
`:memory:` pins its pool to one connection.

| Function | Behavior |
|---|---|
| `db_null/int/float/text/bool(..)` | `DbVal` constructors |
| `db_encode_params(args)` / `db_decode_rows(s)` | Framing codec (length-prefixed `P`/`R` grammar; multibyte-safe) |
| `db_exec(dsn, sql, args)` | Statement; affected rows or `Err` |
| `db_query(dsn, sql, args)` | Rows (`[]` when rowless) or `Err` |
| `db_pool_config(dsn, max, timeout_ms)` | Pool bounds or `Err` |
| `db_close_pool(dsn)` | Retire a pool (idempotent) |

Pinned by `test_db.gzv` (constructors, framing round-trip incl.
separators/multibyte text, CRUD, pool-config errors, unknown
driver — sequential, so it covers the interpreter arms), the
corpus `database/` goldens (`:memory:`, file, concurrent
pool hammer), and the Rust suites (`db_sqlite`,
`db_postgres`, `db_mysql`, `db_pool` pool semantics).

## Execution paths

| Path | Status | Evidence |
|---|---|---|
| Interpreter (`toto run` / `toto test`) | Full suite green | `toto: 18 passed, 0 failed` in `stdlib/`; corpus `proj09`/`proj10` goldens |
| Runtime executor (concurrent) | Green | `test_concurrent.gzv` (pipeline over channels) |
| Built executable (`toto build`) | Output-equivalent | `stdlib` smoke: binary stdout byte-identical to interpreter |
| JIT via `toto dev` | Unsupported | `dev` is single-file scope (`parse_and_check` only); multi-file dev loop is the known carry-forward |

`test_concurrent.gzv` uses the corpus counted-recv idiom (no
for-over-channel, which the corpus never exercises).
