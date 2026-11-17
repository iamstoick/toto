#!/bin/sh
# Fixed benchmark suite (Phase 7, Stage 1): CPU time, startup time,
# and binary size across interpreter / JIT / Cranelift AOT.
#
# Workloads (all real Toto programs): bench/compute.gzv (arithmetic),
# bench/conc.gzv (channel fan-out/fan-in), bench/stdlib.gzv
# (gzv.std math + collections), apps/wordfreq with a scaled input
# (real app: stdlib strings/json/ffi/io + channels + FFI).
# Corpus programs are all millisecond-scale, so the first three are
# calibrated bench workloads (~2-3 s interpreter each); wordfreq is
# the repo-real anchor (stock input runs in ~10 ms, hence the scaled
# copy — method below, repo app untouched).
#
# Paths: interpreter = `toto run` (tree-walker; the runtime executor
# for concurrent programs — noted per workload, never hidden).
# JIT = `jit_probe` (gate mode sequential, `--task` concurrent —
# the same split as the differential suites). `toto run` has NO JIT
# path (interpreter-or-executor only), so JIT numbers come from the
# probe, which compiles every run: reported JIT time is compile+run
# (the timings suite pins compile cost separately).
# AOT = `toto build` output in an isolated project dir.
# LIMIT (recorded): the probe is single-file only (clean E0401 on
# imports), so JIT has no stdlib/wordfreq number. Filling that hole
# needs an import-capable JIT runner — named as a Stage 2 scoping
# input, not built here.
#
# Correctness first: exact output asserted on every path before
# timing (constants for bench/*, cross-path identity for wordfreq).
# A mismatch fails loudly — never timed blind.
#
# Usage: scripts/bench.sh [--runs N] [toto-binary]
# Not part of scripts/check.sh: ~10 min wall time (4 cold AOT
# builds). Run manually per release; the phase report records method
# + numbers. Statistics are median + range over N runs (default 5);
# single runs are never reported as conclusions.
set -eu
RUNS=5
case "${1:-}" in
  --runs) RUNS="$2"; shift 2;;
  --runs=*) RUNS="${1#--runs=}"; shift;;
esac
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOTO="${1:-$ROOT/target/debug/toto}"
PROBE="$ROOT/target/debug/jit_probe"
SAMPLE="${TMPDIR:-/tmp}/bench-aot"
SCALE=2000

# --- correctness gates (expected outputs, recorded 2026-11-17) ---
EXPECT_COMPUTE="1249999000000"
EXPECT_CONC="499999500000"
EXPECT_STDLIB="166666668"

cargo build -q -p toto-cli -p toto-jit --bin jit_probe
TOTO="$ROOT/target/debug/toto"

check_exact() { # $1=name $2=got $3=want
  if [ "$2" != "$3" ]; then
    echo "FAIL $1: output mismatch (got [$2] want [$3])"
    exit 1
  fi
}

# median + range over "$RUNS" runs of "$@" (ms); prints "med [min-max]".
timed() { # $1=label $2..=command
  label="$1"; shift
  times=""
  i=0
  while [ "$i" -lt "$RUNS" ]; do
    start=$(date +%s%N)
    out=$("$@" 2>/dev/null); code=$?
    end=$(date +%s%N)
    if [ "$code" != "0" ]; then echo "FAIL $label: exit $code"; exit 1; fi
    echo "$out" > "/tmp/bench_last_$label.out"
    times="$times $(( (end - start) / 1000000 ))"
    i=$((i + 1))
  done
  # shellcheck disable=SC2086
  stats=$(echo $times | tr ' ' '\n' | sort -n | awk 'NR==1{m=$1} NR==3{med=$1} {x=$1} END{print med" ["m"-"x"]"}')
  echo "$label: ${stats} ms (n=$RUNS)"
}

echo "== correctness: bench programs =="
out=$("$TOTO" run "$ROOT/bench/compute.gzv"); check_exact compute "$out" "$EXPECT_COMPUTE"
out=$("$PROBE" "$ROOT/bench/compute.gzv"); check_exact compute-jit "$out" "$EXPECT_COMPUTE"
out=$("$TOTO" run "$ROOT/bench/conc.gzv"); check_exact conc "$out" "$EXPECT_CONC"
out=$("$PROBE" --task "$ROOT/bench/conc.gzv"); check_exact conc-jit "$out" "$EXPECT_CONC"
out=$("$TOTO" run "$ROOT/bench/stdlib.gzv"); check_exact stdlib "$out" "$EXPECT_STDLIB"
echo "probe import limit (recorded, not a failure):"
"$PROBE" "$ROOT/bench/stdlib.gzv" 2>&1 | head -n 1 || true
echo "ok correctness (interp + jit where runnable)"

echo "== aot projects =="
rm -rf "$SAMPLE"; mkdir -p "$SAMPLE"
mk() { # $1=name $2=src
  mkdir -p "$SAMPLE/$1"
  cp "$2" "$SAMPLE/$1/main.gzv"
  printf '[project]\nname = "%s"\nversion = "0.1.0"\nentry = "main.gzv"\n' "$1" > "$SAMPLE/$1/toto.toml"
}
mk compute "$ROOT/bench/compute.gzv"
mk conc "$ROOT/bench/conc.gzv"
mk stdlib "$ROOT/bench/stdlib.gzv"
cp -r "$ROOT/apps/wordfreq" "$SAMPLE/wordfreq"
(cd "$SAMPLE/wordfreq" && for i in $(seq 1 $SCALE); do cat input.txt; done > input.big && mv input.big input.txt && rm -f report.json)
echo "wordfreq scaled input: $(wc -l < "$SAMPLE/wordfreq/input.txt") lines"

echo "== correctness: aot + cross-path identity =="
for w in compute conc stdlib; do
  (cd "$SAMPLE/$w" && "$TOTO" build >/dev/null 2>&1)
done
(cd "$SAMPLE/wordfreq" && "$TOTO" build >/dev/null 2>&1)
out=$(cd "$SAMPLE/compute" && ./target/compute); check_exact compute-aot "$out" "$EXPECT_COMPUTE"
out=$(cd "$SAMPLE/conc" && ./target/conc); check_exact conc-aot "$out" "$EXPECT_CONC"
out=$(cd "$SAMPLE/stdlib" && ./target/stdlib); check_exact stdlib-aot "$out" "$EXPECT_STDLIB"
(cd "$SAMPLE/wordfreq" && rm -f report.json && "$TOTO" run main.gzv > /tmp/bench_wf_interp.out 2>/dev/null && rm -f report.json)
(cd "$SAMPLE/wordfreq" && rm -f report.json && ./target/wordfreq > /tmp/bench_wf_aot.out 2>/dev/null && rm -f report.json)
if ! cmp -s /tmp/bench_wf_interp.out /tmp/bench_wf_aot.out; then echo "FAIL wordfreq: interp vs aot differ"; exit 1; fi
echo "ok aot correctness (4/4; wordfreq interp==aot)"

echo "== cpu: interpreter (conc + wordfreq via runtime executor) =="
timed interp-compute "$TOTO" run "$ROOT/bench/compute.gzv"
timed interp-conc "$TOTO" run "$ROOT/bench/conc.gzv"
timed interp-stdlib "$TOTO" run "$ROOT/bench/stdlib.gzv"
(cd "$SAMPLE/wordfreq" && timed interp-wordfreq "$TOTO" run main.gzv)

echo "== cpu: jit (compile+run) =="
timed jit-compute "$PROBE" "$ROOT/bench/compute.gzv"
timed jit-conc "$PROBE" --task "$ROOT/bench/conc.gzv"
echo "SKIP jit-stdlib jit-wordfreq (probe is single-file only: E0401)"

echo "== cpu: aot =="
timed aot-compute "$SAMPLE/compute/target/compute"
timed aot-conc "$SAMPLE/conc/target/conc"
timed aot-stdlib "$SAMPLE/stdlib/target/stdlib"
# Wordfreq reads inputs relative to cwd and writes report.json
# there: run inside its project dir (artifacts stay in /tmp).
(cd "$SAMPLE/wordfreq" && timed aot-wordfreq ./target/wordfreq && rm -f report.json)

echo "== startup (trivial program) =="
printf 'fn main() -> int {\n    return 0;\n}\n' > /tmp/bench_trivial.gzv
mkdir -p "$SAMPLE/trivial" && cp /tmp/bench_trivial.gzv "$SAMPLE/trivial/main.gzv"
printf '[project]\nname = "trivial"\nversion = "0.1.0"\nentry = "main.gzv"\n' > "$SAMPLE/trivial/toto.toml"
(cd "$SAMPLE/trivial" && "$TOTO" build >/dev/null 2>&1)
timed start-interp "$TOTO" run /tmp/bench_trivial.gzv
timed start-jit "$PROBE" /tmp/bench_trivial.gzv
timed start-aot "$SAMPLE/trivial/target/trivial"

echo "== binary size (bytes) =="
for w in compute conc stdlib wordfreq trivial; do
  bin="$SAMPLE/$w/target/$w"
  echo "aot-$w: $(wc -c < "$bin")"
done

echo "bench done"
