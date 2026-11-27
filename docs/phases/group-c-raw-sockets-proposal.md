# Group C proposal: raw sockets, packet crafting, capture — SPIKE COMPLETE, BLOCKED

v2.5 items (Group C): raw sockets, packet crafting, packet
capture. Per the v2.5 rule these need their own proposal,
spike, and exit criteria before any implementation. This
note is the proposal + spike record. No code, no spec/ADR
change is added here — and none CAN be yet: the spike finds
this item blocked on privileges and on deferred FFI work,
both outside any single wave's power to resolve.

## Spike verdict: blocked, two independent gates

Gate 1 — privilege (unverifiable in this sandbox, and a
product decision in any case). `SOCK_RAW` requires
`CAP_NET_RAW` (Linux) / root (macOS); capture requires
more (`AF_PACKET` on Linux, `/dev/bpf` + ioctls on
macOS). Nothing in the tree runs privileged, and the
sandbox used for verification cannot grant it — the
privilege story (setuid helper? capability grant at
deploy? root-only tool?) is undecided and undelegable:
it changes who may run toto, not just what toto does.

Gate 2 — deferred FFI (architecture decisions, on
record). `docs/ffi.md` defers, with consequences stated:

- Structs-by-value: "any C API needing struct values is
  inexpressible in v1" — Berkeley Packet Filter ioctls,
  `sockaddr_ll`, and `pcap_pkthdr` all need struct values.
- `dlopen`: "every foreign symbol must be known at binary
  build time" — libpcap cannot be loaded at runtime, so
  capture would hard-link libpcap into every toto binary.

Crafting alone (without capture) is the narrower
reading: IP/TCP headers as `Array<int>` via the existing
bytes ops + a `socket(AF_INET, SOCK_RAW)` constructor would
dodge the FFI gates — but NOT the privilege gate
(`IP_HDRINCL` still needs `CAP_NET_RAW`/root), so the
narrower reading stays blocked too.

## Proposed shape (for the wave that follows unblocking)

- Privilege decision first (user-owned): document the
  supported grant model per platform before any code.
- Then, in dependency order: (a) FFI unblocking IF the
  design needs struct values or runtime-loaded pcap
  (alternatively: hard-link libpcap on the platforms that
  ship it — documented tradeoff, no new FFI power); (b)
  `stdlib/packet.gzv`: header build/parse over bytes ops
  (pure toto, fully golden-testable with zero privilege);
  (c) raw send/recv builtins behind the privilege gate,
  refused with a fixed-string error when unprivileged
  (never a panic, never silent downgrade).
- Determinism: bytes-level codec goldens (craft/parse
  vectors) run everywhere; live send/capture tests stay
  behind a privilege-gated harness (skipped without the
  grant, like the network tests without a peer).

## Exit criteria (nothing ships until all hold)

1. Privilege model documented and user-accepted.
2. Codec goldens (craft/parse, IPv4/TCP/UDP/ICMP
   vectors) green on all platforms.
3. Unprivileged refusal tests green (fixed strings).
4. Live send/capture against a loopback peer, gated by
   the grant, green where the grant exists.
5. Full gate green + CI green.
