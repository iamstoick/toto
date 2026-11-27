# Group C proposal: WebSocket client (`ws://`) — SPIKE COMPLETE, NOT STARTED

v2.5 item (Group C): WebSockets. Per the v2.5 rule this item
needs its own proposal, spike, and exit criteria before any
implementation. This note is the proposal + spike record. No
code, no spec/ADR change, no builtin is added here.

## Spike verdict: feasible, one wave, two gaps

A `ws://` client can be built on substrate that already
exists; exactly two gaps need closing (both small, both
wave work — hence no implementation in this turn).

Present today (verified in tree this turn):

- TCP: `net_tcp_connect` / `net_send_all` / `net_recv` /
  `net_close` (`stdlib/net.gzv`) — the handshake and the
  frame stream run over these unchanged.
- base64: `bytes_b64_encode` (`stdlib/bytes.gzv`) for the
  `Sec-WebSocket-Key` / `Accept` headers.
- HTTP parse helpers (`stdlib/http.gzv`) for the 101
  response line + header check.
- Crypto backend pattern: one-shot OpenSSL calls through
  `cryptolib`, system libcrypto already linked
  (`crates/toto-compiler/src/cryptolib.rs:49`).

Gap 1 — SHA-1 (required by RFC 6455 §1.3, no substitute):
`Sec-WebSocket-Accept = base64(SHA-1(key + GUID))` and
nothing in the tree computes SHA-1 (14 `toto_rt_crypto_*`
externs: sha256/512, hmac, aead, sign, random, pbkdf2, rsa,
x509 — no sha1, verified this turn). Close: one more
one-shot `toto_rt_crypto_sha1` in the established pattern
(no new link, no retained context). Pure-toto SHA-1 is not
proposed — the handshake hash belongs beside the other
digests.

Gap 2 — frame masking (RFC 6455 §5.3): client frames MUST
be masked (payload XOR 4-byte key) and the language has no
bitwise XOR and no shifts (`BinOp` is logical `And`/`Or`
plus arithmetic/comparison — verified this turn), so the
mask loop cannot be written in pure toto today. Close
either as a `toto_rt_ws_mask` runtime helper owned by the
wave, or as new builtins — either way it touches the
frozen 27-name builtin catalog thinking and needs its own
spec line, so it is wave work, not a drive-by.

Out of scope for the first wave: `wss://` (TLS-wrapped
frames — needs the `tlslib` session under a ws frame
layer, a second wave), servers (accept + handshake
validation), compression (`permessage-deflate`), and any
spec change beyond the masking line.

## Proposed shape (for the implementing wave)

- `stdlib/ws.gzv` (`gzv.std/ws`): `ws_connect(url)` over
  `net_tcp_*` (blocking pool, never the poller), handshake
  build/verify, `ws_send_text/bytes`, `ws_recv` (one frame;
  control-frame handling: pong auto-reply, close echo),
  `ws_close`. Errors as `Result` `Err` with fixed strings
  (golden-pinnable, per the crypto discipline).
- Determinism: network I/O is timing-shaped, so coverage
  and corpus goldens pin the codec (handshake string,
  frame encode/decode vectors incl. 16-bit/64-bit lengths
  and masked round-trips), never a live peer. Live-peer
  tests stay behind the existing network-gated harness.

## Exit criteria (nothing ships until all hold)

1. Codec goldens: masked + unmasked vectors, all three
   length classes, close/ping/pong handling, all green.
2. Live interop against a local reference server
   (RFC 6455 handshake + echo, text + binary), gated like
   the other network tests (skipped without a peer).
3. `toto coverage` still refuses nothing new; no existing
   golden moves (new module only, one cryptolib op).
4. Full gate green + CI green.
