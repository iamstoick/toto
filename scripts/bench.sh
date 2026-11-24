#!/bin/sh
# Fixed benchmark suite (Phase 7, Stage 1): CPU time, startup time,
# and binary size across interpreter / JIT / Cranelift AOT.
#
# Workloads (all real Toto programs): bench/compute.gzv (arithmetic),
# bench/conc.gzv (channel fan-out/fan-in), bench/stdlib.gzv
# (gzv.std math + collections), apps/wordfreq with a scaled input
# (real app: stdlib strings/json/ffi/io + channels + FFI).
# tests/corpus/project/proj25_refsvc_data/load.gzv (reference
# service load: pooled DB + authed TLS service, sequential
# client; pool contention proven concurrently in-process).
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
# imports), so JIT has no stdlib number (Stage 2 decision:
# record-the-limit; stdlib-JIT stays N/A). wordfreq JIT-task IS
# measured outside this script by the zz_ffidiag diagnostic
# (6000-line input, task mode), cited in the phase report.
#
# Correctness first: exact output asserted on every path before
# timing (constants for bench/*, cross-path identity for wordfreq).
# A mismatch fails loudly — never timed blind.
#
# Usage: scripts/bench.sh [--runs N] [--baseline FILE] [toto-binary]
# Not part of scripts/check.sh: ~25 min wall time (4 cold debug + 5
# cold release AOT links; needs ~4 GB free in $TMPDIR). Run manually
# per release; the phase report records method + numbers. Statistics
# are median + range over N runs (default 5); single runs are never
# reported as conclusions.
# With --baseline FILE (bench/baseline.txt committed), every timed
# label is compared against the recorded median after the run: any
# median above 5x its baseline prints FLAG and fails the suite
# (exit 1). The 5x rule absorbs CI machine variance while catching
# order-of-magnitude regressions (the wordfreq 75s anomaly was
# ~500x). CI runs the full suite at --runs 3 with this flag; the
# manual n=5 run stays the source of published numbers.
set -eu
RUNS=5
BASELINE=""
case "${1:-}" in
  --runs) RUNS="$2"; shift 2;;
  --runs=*) RUNS="${1#--runs=}"; shift;;
esac
case "${1:-}" in
  --baseline) BASELINE="$2"; shift 2;;
  --baseline=*) BASELINE="${1#--baseline=}"; shift;;
esac
if [ -n "$BASELINE" ] && [ ! -f "$BASELINE" ]; then
  echo "FAIL: baseline file not found: $BASELINE"
  exit 1
fi
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOTO="${1:-$ROOT/target/debug/toto}"
PROBE="$ROOT/target/debug/jit_probe"
SAMPLE="${TMPDIR:-/tmp}/bench-aot"
SCALE=2000

# --- correctness gates (expected outputs, recorded 2026-11-17) ---
EXPECT_COMPUTE="1249999000000"
EXPECT_CONC="499999500000"
EXPECT_STDLIB="166666668"
EXPECT_GC_ALLOC="59760000"
EXPECT_GC_CHAN="$(printf '21943430\n2621250')"

cargo build -q -p toto-cli -p toto-jit --bin jit_probe
TOTO="$ROOT/target/debug/toto"

check_exact() { # $1=name $2=got $3=want
  if [ "$2" != "$3" ]; then
    echo "FAIL $1: output mismatch (got [$2] want [$3])"
    exit 1
  fi
}

# median + range over "$RUNS" runs of "$@" (ms); prints "med [min-max]".
# Every median is also appended to $MEDIANS for --baseline flagging.
MEDIANS="$SAMPLE/bench_medians.txt"
rm -f "$MEDIANS"
timed() { # $1=label $2..=command
  label="$1"; shift
  times=""
  i=0
  while [ "$i" -lt "$RUNS" ]; do
    start=$(date +%s%N)
    out=$("$@" 2>/dev/null); code=$?
    end=$(date +%s%N)
    if [ "$code" != "0" ]; then echo "FAIL $label: exit $code"; exit 1; fi
    echo "$out" > "$SAMPLE/bench_last_$label.out"
    times="$times $(( (end - start) / 1000000 ))"
    i=$((i + 1))
  done
  # shellcheck disable=SC2086
  stats=$(echo $times | tr ' ' '\n' | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]" ["a[1]"-"a[NR]"]"}')
  echo "$label: ${stats} ms (n=$RUNS)"
  med=${stats%% *}
  echo "$label $med" >> "$MEDIANS"
}

echo "== correctness: bench programs =="
out=$("$TOTO" run "$ROOT/bench/compute.gzv"); check_exact compute "$out" "$EXPECT_COMPUTE"
out=$("$PROBE" "$ROOT/bench/compute.gzv"); check_exact compute-jit "$out" "$EXPECT_COMPUTE"
out=$("$TOTO" run "$ROOT/bench/conc.gzv"); check_exact conc "$out" "$EXPECT_CONC"
out=$("$PROBE" --task "$ROOT/bench/conc.gzv"); check_exact conc-jit "$out" "$EXPECT_CONC"
out=$("$TOTO" run "$ROOT/bench/stdlib.gzv"); check_exact stdlib "$out" "$EXPECT_STDLIB"
# GC workloads run on the runtime executor only (the tree-walker
# cannot run concurrent programs; the JIT probe is single-file).
out=$("$TOTO" run "$ROOT/bench/gc_alloc.gzv"); check_exact gc-alloc "$out" "$EXPECT_GC_ALLOC"
out=$("$TOTO" run "$ROOT/bench/gc_chan.gzv"); check_exact gc-chan "$out" "$EXPECT_GC_CHAN"
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
(cd "$SAMPLE/wordfreq" && cp input.txt input.stock && for i in $(seq 1 $SCALE); do cat input.stock; done > input.txt && rm -f report.json)
echo "wordfreq scaled input: $(wc -l < "$SAMPLE/wordfreq/input.txt") lines"

echo "== correctness: aot + cross-path identity =="
for w in compute conc stdlib; do
  (cd "$SAMPLE/$w" && "$TOTO" build >/dev/null 2>&1)
done
(cd "$SAMPLE/wordfreq" && "$TOTO" build >/dev/null 2>&1)
out=$(cd "$SAMPLE/compute" && ./target/compute); check_exact compute-aot "$out" "$EXPECT_COMPUTE"
out=$(cd "$SAMPLE/conc" && ./target/conc); check_exact conc-aot "$out" "$EXPECT_CONC"
out=$(cd "$SAMPLE/stdlib" && ./target/stdlib); check_exact stdlib-aot "$out" "$EXPECT_STDLIB"
(cd "$SAMPLE/wordfreq" && rm -f report.json && "$TOTO" run main.gzv > $SAMPLE/bench_wf_interp.out 2>/dev/null && rm -f report.json)
(cd "$SAMPLE/wordfreq" && rm -f report.json && ./target/wordfreq > $SAMPLE/bench_wf_aot.out 2>/dev/null && rm -f report.json)
if ! cmp -s $SAMPLE/bench_wf_interp.out $SAMPLE/bench_wf_aot.out; then echo "FAIL wordfreq: interp vs aot differ"; exit 1; fi
echo "ok aot correctness (4/4; wordfreq interp==aot)"

echo "== cpu: interpreter (conc + wordfreq via runtime executor) =="
timed interp-compute "$TOTO" run "$ROOT/bench/compute.gzv"
timed interp-conc "$TOTO" run "$ROOT/bench/conc.gzv"
timed interp-stdlib "$TOTO" run "$ROOT/bench/stdlib.gzv"
(cd "$SAMPLE/wordfreq" && timed interp-wordfreq "$TOTO" run main.gzv)

echo "== cpu: gc workloads (runtime executor; STW collector under test) =="
timed exec-gc-alloc "$TOTO" run "$ROOT/bench/gc_alloc.gzv"
timed exec-gc-chan "$TOTO" run "$ROOT/bench/gc_chan.gzv"

echo "== refsvc load (runtime executor; pooled DB + authed TLS service) =="
# The service reads fixtures + service.toml relative to its project
# dir, so everything runs there; the DB file lives in $SAMPLE.
# Correctness first: the program gates itself (nonzero exit on any
# gate failure) and prints its verdict + LOAD line; fail loudly.
(
cd "$ROOT/tests/corpus/project/proj25_refsvc_data"
export TOTO_REFSVC_PORT=48527
export TOTO_REFSVC_DSN="sqlite:path=$SAMPLE/toto-refsvc-bench.db"
export TOTO_REFSVC_REQS=60
"$TOTO" run load.gzv > "$SAMPLE/bench_refsvc.out" 2>&1
grep -q "LOAD-OK" "$SAMPLE/bench_refsvc.out" || { echo "FAIL refsvc-load: no LOAD-OK"; exit 1; }
grep -q "^phase-c ok=" "$SAMPLE/bench_refsvc.out" || { echo "FAIL refsvc-load: no phase-c line"; exit 1; }
grep -hE "^(LOAD|recover|phase-a)" "$SAMPLE/bench_refsvc.out" | tail -n 4
timed exec-refsvc-load "$TOTO" run load.gzv
)
rm -f "$SAMPLE"/toto-refsvc-bench.db*

echo "== refsvc load with profiler on (100 Hz sampling + heap tally) =="
# Overhead backstop for Stage 2: same 60-req conc-2 shape with
# TOTO_PROFILE=1. Asserts LOAD-OK (profiling must not break the
# service) and records wall-clock for the 5x flag rule. The tight
# overhead number (<1% on the deterministic proj26 shape) lives
# in the Wave 4 report; this row catches order-of-magnitude
# regressions in CI.
(
cd "$ROOT/tests/corpus/project/proj25_refsvc_data"
export TOTO_REFSVC_PORT=48527
export TOTO_REFSVC_DSN="sqlite:path=$SAMPLE/toto-refsvc-bench.db"
export TOTO_REFSVC_REQS=60
export TOTO_PROFILE=1
export TOTO_PROFILE_OUT="$SAMPLE/bench_refsvc.prof"
export TOTO_PROFILE_HZ=100
"$TOTO" run load.gzv > "$SAMPLE/bench_refsvc_prof.out" 2>&1
grep -q "LOAD-OK" "$SAMPLE/bench_refsvc_prof.out" || { echo "FAIL refsvc-load-prof: no LOAD-OK"; exit 1; }
grep -q "^profile v1" "$SAMPLE/bench_refsvc.prof" || { echo "FAIL refsvc-load-prof: no profile"; exit 1; }
grep -hE "^(LOAD|recover|phase-a)" "$SAMPLE/bench_refsvc_prof.out" | tail -n 4
timed exec-refsvc-load-prof "$TOTO" run load.gzv
)
rm -f "$SAMPLE"/toto-refsvc-bench.db* "$SAMPLE"/bench_refsvc_prof.out "$SAMPLE"/bench_refsvc.prof

echo "== cpu: jit (compile+run) =="
timed jit-compute "$PROBE" "$ROOT/bench/compute.gzv"
timed jit-conc "$PROBE" --task "$ROOT/bench/conc.gzv"
echo "SKIP jit-stdlib jit-wordfreq (probe is single-file only: E0401)"

echo "== cpu: aot (debug/Off) =="
timed aot-compute "$SAMPLE/compute/target/compute"
timed aot-conc "$SAMPLE/conc/target/conc"
timed aot-stdlib "$SAMPLE/stdlib/target/stdlib"
# Wordfreq reads inputs relative to cwd and writes report.json
# there: run inside its project dir (artifacts stay in /tmp).
(cd "$SAMPLE/wordfreq" && timed aot-wordfreq ./target/wordfreq && rm -f report.json)
# Debug sizes before the release rebuilds overwrite the binaries.
# Size rows also feed $MEDIANS (same 5x flag rule as timings):
# `size-aot-*` are debug binaries, `size-aotrel-*` release.
: > $SAMPLE/bench_sizes_debug.txt
for w in compute conc stdlib wordfreq; do
  sz=$(wc -c < "$SAMPLE/$w/target/$w")
  echo "debug-size-$w: $sz" >> $SAMPLE/bench_sizes_debug.txt
  echo "size-aot-$w $sz" >> "$MEDIANS"
done

echo "== aot release builds (production opt: Speed + release link) =="
for w in compute conc stdlib; do
  (cd "$SAMPLE/$w" && "$TOTO" build --release >/dev/null 2>&1)
done
(cd "$SAMPLE/wordfreq" && "$TOTO" build --release >/dev/null 2>&1)

echo "== correctness: aot release =="
check_exact compute-aotrel "$("$SAMPLE/compute/target/compute")" "$EXPECT_COMPUTE"
check_exact conc-aotrel "$("$SAMPLE/conc/target/conc")" "$EXPECT_CONC"
check_exact stdlib-aotrel "$("$SAMPLE/stdlib/target/stdlib")" "$EXPECT_STDLIB"
(cd "$SAMPLE/wordfreq" && rm -f report.json && ./target/wordfreq > $SAMPLE/bench_wf_aotrel.out 2>/dev/null && rm -f report.json)
if ! cmp -s $SAMPLE/bench_wf_interp.out $SAMPLE/bench_wf_aotrel.out; then echo "FAIL wordfreq: interp vs aot-release differ"; exit 1; fi
echo "ok aot release correctness (4/4)"

echo "== cpu: aot release (Speed, production opt) =="
timed aotrel-compute "$SAMPLE/compute/target/compute"
timed aotrel-conc "$SAMPLE/conc/target/conc"
timed aotrel-stdlib "$SAMPLE/stdlib/target/stdlib"
(cd "$SAMPLE/wordfreq" && timed aotrel-wordfreq ./target/wordfreq && rm -f report.json)

echo "== wordfreq input-size series (FFI-path scaling; all 3 paths) =="
# Rebuild debug (the release step overwrote the binary); the series
# then times interp + aot-debug + aot-release on the same inputs.
(cd "$SAMPLE/wordfreq" && cp target/wordfreq target/wordfreq-release && "$TOTO" build >/dev/null 2>&1)
for rep in 1 100 1000 2000; do
  lines=$((6 * rep))
  (cd "$SAMPLE/wordfreq" && rm -f report.json && for i in $(seq 1 $rep); do cat input.stock; done > input.txt)
  (cd "$SAMPLE/wordfreq" && "$TOTO" run main.gzv > $SAMPLE/bench_wf_series_interp.out 2>/dev/null; rm -f report.json)
  (cd "$SAMPLE/wordfreq" && ./target/wordfreq > $SAMPLE/bench_wf_series_aot.out 2>/dev/null; rm -f report.json)
  (cd "$SAMPLE/wordfreq" && ./target/wordfreq-release > $SAMPLE/bench_wf_series_aotrel.out 2>/dev/null; rm -f report.json)
  if ! cmp -s $SAMPLE/bench_wf_series_interp.out $SAMPLE/bench_wf_series_aot.out; then echo "FAIL wordfreq series $lines: interp vs aot differ"; exit 1; fi
  if ! cmp -s $SAMPLE/bench_wf_series_interp.out $SAMPLE/bench_wf_series_aotrel.out; then echo "FAIL wordfreq series $lines: interp vs aot-release differ"; exit 1; fi
  (cd "$SAMPLE/wordfreq" && timed "interp-wf-$lines" "$TOTO" run main.gzv; rm -f report.json)
  (cd "$SAMPLE/wordfreq" && timed "aot-wf-$lines" ./target/wordfreq; rm -f report.json)
  (cd "$SAMPLE/wordfreq" && timed "aotrel-wf-$lines" ./target/wordfreq-release; rm -f report.json)
done
(cd "$SAMPLE/wordfreq" && cp input.stock input.txt && rm -f report.json)
echo "ok wordfreq series (4/4 sizes, interp==aot==aot-release)"

echo "== startup (trivial program) =="
printf 'fn main() -> int {\n    return 0;\n}\n' > $SAMPLE/bench_trivial.gzv
mkdir -p "$SAMPLE/trivial" && cp $SAMPLE/bench_trivial.gzv "$SAMPLE/trivial/main.gzv"
printf '[project]\nname = "trivial"\nversion = "0.1.0"\nentry = "main.gzv"\n' > "$SAMPLE/trivial/toto.toml"
(cd "$SAMPLE/trivial" && "$TOTO" build >/dev/null 2>&1)
timed start-interp "$TOTO" run $SAMPLE/bench_trivial.gzv
timed start-jit "$PROBE" $SAMPLE/bench_trivial.gzv
timed start-aot "$SAMPLE/trivial/target/trivial"
(cd "$SAMPLE/trivial" && "$TOTO" build --release >/dev/null 2>&1)
timed start-aotrel "$SAMPLE/trivial/target/trivial"
# Real-app startup: wordfreq release binary on the 6-line stock
# input (input.txt restored by the series cleanup above).
(cd "$SAMPLE/wordfreq" && timed start-aotrel-wordfreq ./target/wordfreq-release && rm -f report.json)

echo "== binary size (bytes) =="
cat $SAMPLE/bench_sizes_debug.txt
for w in compute conc stdlib trivial; do
  sz=$(wc -c < "$SAMPLE/$w/target/$w")
  echo "aotrel-$w: $sz"
  echo "size-aotrel-$w $sz" >> "$MEDIANS"
done
sz=$(wc -c < "$SAMPLE/wordfreq/target/wordfreq-release")
echo "aotrel-wordfreq: $sz"
echo "size-aotrel-wordfreq $sz" >> "$MEDIANS"
rm -f "$SAMPLE/wordfreq/target/wordfreq-release"

if [ -n "$BASELINE" ]; then
  echo "== regression flagging vs $BASELINE (fail above 5x) =="
  awk -v flag=5 '
    NR==FNR { if ($1 !~ /^#/) base[$1]=$2; next }
    { if ($1 in base) {
        r = ($2 > 0 && base[$1] > 0) ? $2/base[$1] : 0;
        printf "%-22s base=%-8s med=%-8s x%.1f %s\n", $1, base[$1], $2, r, (r > flag ? "FLAG" : "ok");
        if (r > flag) bad = 1;
      } else {
        printf "%-22s med=%-8s (no baseline)\n", $1, $2;
      }
    }
    END { exit bad }
  ' "$BASELINE" "$MEDIANS" || { echo "FAIL: regression flagged above"; exit 1; }
  echo "ok regressions: none above 5x"
fi

echo "bench done"
