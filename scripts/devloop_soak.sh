#!/bin/sh
# Dev-loop survival soak (Phase 6 flag d): drives a real `toto dev`
# watch loop through 100 consecutive edits — 80 good (each awaited
# as a new generation) + 20 broken (each awaited as an error
# notice) — and asserts every generation ran its edit's content.
#
# Usage: scripts/devloop_soak.sh [toto-binary]
# Not part of scripts/check.sh: ~3 minutes wall time with
# poll-based waits. Run manually per release; the Phase 6 report
# records the method and the observed numbers.
set -eu
TOTO="${1:-$(dirname "$0")/../target/debug/toto}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/devloop.XXXXXX")"
# Preserve the work dir (with dev.log) on failure for diagnosis.
PASS=""
trap '[ -n "$PASS" ] && rm -rf "$WORK"' EXIT INT TERM
cd "$WORK"
echo "work: $WORK"

good_prog() { printf 'fn main() -> int {\n    println(%d);\n    return 0;\n}\n' "$1"; }
good_prog 0 > prog.gzv
rm -f dev.log
"$TOTO" dev --drain-timeout 1 prog.gzv > dev.log 2>&1 &
DEV=$!
wait_for() { # $1=pattern $2=timeout_s
  t=0
  while ! grep -q "$1" dev.log 2>/dev/null; do
    sleep 0.2; t=$((t + 1))
    if [ "$t" -ge "$(( $2 * 5 ))" ]; then echo "TIMEOUT waiting for: $1"; kill -TERM $DEV 2>/dev/null || true; exit 1; fi
    kill -0 $DEV 2>/dev/null || { echo "DEV DIED waiting for: $1"; tail -5 dev.log; exit 1; }
  done
  # Settle past the 100 ms watcher poll so the next write lands in
  # its own poll window (no coalescing) and the generation's output
  # lands before the next edit (no preemption).
  sleep 0.5
}
# Start-marker waits only. A generation's output and its `exited`
# marker stream on RETIRE (the next rebuild) by design — atomic
# per-generation output — so the last generation's output is
# unobservable without a further edit; the expected sequence below
# covers generations 1-80, flushed by generations 2-81. Waiting for
# an exit marker before the next write would deadlock (the exit
# marker itself needs that write). Rapid-overwrite preemption is
# covered by the Phase 4 drain tests, not here.
wait_for "generation 1 started" 60
gen=1; errors=0
for i in $(seq 1 100); do
  if [ $(( i % 5 )) -eq 0 ]; then
    printf 'fn main() -> int {\n    println(%d);\n    return ;\n}\n' "$i" > prog.gzv
    # `|| true`: `grep -c` exits 1 on zero matches; the count is
    # what matters, not the exit status.
    n=$(grep -c "source has errors" dev.log || true)
    t=0
    while [ "$(grep -c 'source has errors' dev.log || true)" -le "$n" ]; do
      sleep 0.2; t=$((t + 1))
      kill -0 $DEV 2>/dev/null || { echo "DEV DIED on broken edit $i"; tail -5 dev.log; exit 1; }
      if [ "$t" -ge 50 ]; then echo "TIMEOUT on broken edit $i"; tail -5 dev.log; kill -TERM $DEV 2>/dev/null || true; exit 1; fi
    done
    errors=$((errors + 1))
  else
    good_prog "$i" > prog.gzv
    gen=$((gen + 1))
    wait_for "generation $gen started" 30
  fi
done
[ "$(grep -c 'generation .* started' dev.log || true)" = "81" ] || { echo "want 81 generations"; exit 1; }
[ "$errors" = "20" ] || { echo "want 20 error notices, got $errors"; exit 1; }
# Every generation printed exactly its edit's value (initial 0 plus
# each good edit except the last: generation 81's output streams
# only on retire, and the loop ends by killing dev before any 82nd
# rebuild — unobservable by design, not a skip. The settle delay
# keeps each write in its own poll window, so no generation is
# preempted or coalesced.
want="0 1 2 3 4 6 7 8 9 11 12 13 14 16 17 18 19 21 22 23 24 26 27 28 29 31 32 33 34 36 37 38 39 41 42 43 44 46 47 48 49 51 52 53 54 56 57 58 59 61 62 63 64 66 67 68 69 71 72 73 74 76 77 78 79 81 82 83 84 86 87 88 89 91 92 93 94 96 97 98"
got=$(grep -E '^[0-9]+$' dev.log | tr '\n' ' ' | sed 's/ $//')
[ "$got" = "$want" ] || { echo "output sequence mismatch"; echo "got: $got"; exit 1; }
kill -TERM $DEV 2>/dev/null || true
wait $DEV 2>/dev/null || true
PASS=1
echo "devloop soak: 81 generations, 20 error notices, exact output sequence — PASS"
