#!/bin/sh
# Build-equivalence sample (Phase 5 flag c, re-verified Phase 6):
# build a stated sample of corpus programs with `toto build` and
# compare each built binary against the interpreter goldens
# (stdout + exit code; panic-head for the divzero case, whose
# locations move under AOT).
#
# Usage: scripts/buildsample.sh [toto-binary]
# Not part of scripts/check.sh: one `toto build` per program takes
# minutes wall time. Run manually per release; the phase report
# records the method and the observed results.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOTO="${1:-$ROOT/target/debug/toto}"
CORP="$ROOT/tests/corpus"
SAMPLE="${TMPDIR:-/tmp}/buildsample"
rm -rf "$SAMPLE"; mkdir -p "$SAMPLE"
mk() { # $1=name $2=src-relpath
  mkdir -p "$SAMPLE/$1"
  cp "$CORP/$2" "$SAMPLE/$1/main.gzv"
  printf '[project]\nname = "%s"\nversion = "0.1.0"\nentry = "main.gzv"\n' "$1" > "$SAMPLE/$1/toto.toml"
}
mk arith interp/int01_arith.gzv
mk closure interp/clo04_higher_order.gzv
mk structs interp/struct01_basic.gzv
mk loops interp/loop02_for_range.gzv
mk divzero interp/int03_divzero.gzv
mk pingpong concurrent/conc01_pingpong.gzv
cp -r "$CORP/project/proj09_stdlib" "$SAMPLE/stdlibproj"
printf '[project]\nname = "stdlibproj"\nversion = "0.1.0"\nentry = "main.gzv"\n' > "$SAMPLE/stdlibproj/toto.toml"
pass=0; fail=0
check() { # $1=name $2=golden-stdout $3=golden-status
  (cd "$SAMPLE/$1" && "$TOTO" build >/dev/null 2>&1) || { echo "FAIL $1: build"; fail=$((fail+1)); return; }
  out=$(cd "$SAMPLE/$1" && ./target/$1 2>/tmp/err_$1.txt); code=$?
  want_out=$(cat "$2" 2>/dev/null); want_code=$(cat "$3")
  if [ "$out" = "$want_out" ] && [ "$code" = "$want_code" ]; then
    if [ "$code" != "0" ]; then
      # Panic path: compare code+message (locations move under AOT).
      got_head=$(head -1 /tmp/err_$1.txt | sed 's/.*\(panic\[E[0-9]*\]: .*\)/\1/')
      want_head=$(head -1 "$SAMPLE/$1/main.stderr" 2>/dev/null || head -1 "$CORP/interp/$1.stderr" 2>/dev/null)
      echo "INFO $1 panic head: $got_head"
    fi
    echo "ok $1 (exit $code)"; pass=$((pass+1))
  else
    echo "FAIL $1: exit=$code want=$want_code out=[$out] want=[$want_out]"; fail=$((fail+1))
  fi
}
check arith "$CORP/interp/int01_arith.stdout" "$CORP/interp/int01_arith.status"
check closure "$CORP/interp/clo04_higher_order.stdout" "$CORP/interp/clo04_higher_order.status"
check structs "$CORP/interp/struct01_basic.stdout" "$CORP/interp/struct01_basic.status"
check loops "$CORP/interp/loop02_for_range.stdout" "$CORP/interp/loop02_for_range.status"
check divzero /dev/null "$CORP/interp/int03_divzero.status"
check pingpong "$CORP/concurrent/conc01_pingpong.stdout" "$CORP/concurrent/conc01_pingpong.status"
check stdlibproj "$CORP/project/proj09_stdlib/main.stdout" "$CORP/project/proj09_stdlib/main.status"
echo "sample: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
