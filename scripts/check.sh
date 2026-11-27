#!/bin/sh
# Repo gate: formatting, lints, tests. CI (.github/workflows/ci.yml)
# runs this script on push and pull request, Linux x86_64.
set -eu
cd "$(dirname "$0")/.."
# Fall back to a user-local toolchain when cargo is not on PATH.
if ! command -v cargo >/dev/null 2>&1; then
    for d in "${CARGO_HOME:-$HOME/.cargo}/bin" "$HOME/.cargo/bin"; do
        if [ -x "$d/cargo" ]; then
            PATH="$d:$PATH"
            break
        fi
    done
fi
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
# --nocapture: test stdout (notably the JIT compile-time table) is
# preserved in CI logs for the phase report.
cargo test --workspace -- --nocapture
# stdlib suite: the toolchain's own test project must stay green.
# (`toto test` exits 1 on any failure; `set -eu` fails the gate.)
cargo build -p toto-cli
# Stage 1 lint gate (Wave 5): Tier-A/B hygiene over the repo's own
# programs. Error-goldens (sibling .status != 0) are not lintable
# programs and are skipped; `tests/lint-exceptions.txt` lists the
# reasoned exclusions (shared with the corpus_tier_a_clean proof).
lint_files=$(find tests/corpus stdlib bench apps -name '*.gzv' | sort)
lint_targets=""
for f in $lint_files; do
    case "$f" in
        tests/corpus/*)
            status_file="${f%.gzv}.status"
            if [ -f "$status_file" ] && [ "$(cat "$status_file")" != "0" ]; then
                continue
            fi
            ;;
    esac
    if grep -qxF -e "$f" tests/lint-exceptions.txt; then
        continue
    fi
    lint_targets="$lint_targets $f"
done
# shellcheck disable=SC2086
./target/debug/toto lint --deny-warnings $lint_targets
# Stage 3 coverage gate (Wave 5): the LCOV report for the demo
# program must match the checked-in golden byte for byte
# (SF line normalized: only the file name is stable), and the
# --fail-under gate shape is exercised at the demo's own level.
./target/debug/toto coverage crates/toto-cli/tests/fixtures/coverage/branches.gzv 2>/dev/null | sed 's|^SF:.*/\(.*\)|SF:\1|' > /tmp/toto-cov-demo.lcov
cmp /tmp/toto-cov-demo.lcov crates/toto-cli/tests/fixtures/coverage/branches.gzv.lcov
./target/debug/toto coverage --fail-under 100 crates/toto-cli/tests/fixtures/coverage/branches.gzv >/dev/null
# Wave 8 concurrent coverage gate: each concurrent fixture's LCOV
# must match its golden, and two back-to-back runs must agree byte
# for byte (workers = 1, fixed seed — the determinism contract).
for conc in conc conc_select conc_branches; do
    fix="crates/toto-cli/tests/fixtures/coverage/$conc.gzv"
    ./target/debug/toto coverage "$fix" 2>/dev/null | sed 's|^SF:.*/\(.*\)|SF:\1|' > /tmp/toto-cov-conc.lcov
    cmp /tmp/toto-cov-conc.lcov "$fix.lcov"
    ./target/debug/toto coverage "$fix" 2>/dev/null > /tmp/toto-cov-conc-a.lcov
    ./target/debug/toto coverage "$fix" 2>/dev/null > /tmp/toto-cov-conc-b.lcov
    cmp /tmp/toto-cov-conc-a.lcov /tmp/toto-cov-conc-b.lcov
done
# Wave 9 registry gate: index curation round trip (offline, no
# git) plus release cut + idempotent re-release + push of one
# tag to a local bare remote (offline, hermetic). Needs git.
TOTO_BIN="$PWD/target/debug/toto"
rm -rf /tmp/toto-reg-gate && mkdir -p /tmp/toto-reg-gate
"$TOTO_BIN" init /tmp/toto-reg-gate/calc >/dev/null
(cd /tmp/toto-reg-gate/calc && "$TOTO_BIN" index add leftpad https://example.com/leftpad >/dev/null)
grep -qxF 'leftpad = "https://example.com/leftpad"' /tmp/toto-reg-gate/calc/toto-index.toml
(cd /tmp/toto-reg-gate/calc && "$TOTO_BIN" index list | grep -qxF 'leftpad = "https://example.com/leftpad"')
(cd /tmp/toto-reg-gate/calc && "$TOTO_BIN" index remove leftpad >/dev/null)
# Removed entries must be gone from list output (grep finds
# nothing → the && does not fire; a leftover fails the gate).
(cd /tmp/toto-reg-gate/calc && "$TOTO_BIN" index list | grep -q leftpad) && exit 1
(cd /tmp/toto-reg-gate/calc && git init -q . && git add -A && git -c user.email=t@t -c user.name=t commit -qm init && "$TOTO_BIN" release > /tmp/toto-rel-1.txt)
grep -q "released calc 0.1.0 (tag v0.1.0)" /tmp/toto-rel-1.txt
test -f /tmp/toto-reg-gate/calc/calc-0.1.0.gzv-pkg
(cd /tmp/toto-reg-gate/calc && "$TOTO_BIN" release > /tmp/toto-rel-2.txt)
grep -q "already released calc 0.1.0" /tmp/toto-rel-2.txt
(cd /tmp/toto-reg-gate/calc && git init -q --bare ../remote.git && git remote add origin ../remote.git && git push -q origin HEAD:main && git branch --set-upstream-to=origin/main -q && "$TOTO_BIN" release --push > /tmp/toto-rel-3.txt)
grep -q "pushed v0.1.0 to origin" /tmp/toto-rel-3.txt
git ls-remote /tmp/toto-reg-gate/remote.git | grep -q "refs/tags/v0.1.0"
# Stage 4 doc gate (Wave 5): `toto doc` renders every stdlib module
# (parse refusal would fail here), and every `pub` fn/struct has a
# `###` signature heading in its module file (surface completeness,
# self-maintaining: no hardcoded counts).
rm -rf /tmp/toto-doc-gate
./target/debug/toto doc --out /tmp/toto-doc-gate stdlib/*.gzv
: > /tmp/toto-doc-missing.txt
for f in stdlib/*.gzv; do
    name=$(basename "$f" .gzv)
    grep -hoE '^pub (unsafe )?fn [A-Za-z0-9_]+|^pub struct [A-Za-z0-9_]+' "$f" | sed -E 's/^pub (unsafe )?fn //;s/^pub struct //' | while read -r item; do
        grep -qxF "### $item" /tmp/toto-doc-gate/"$name.md" || echo "$name.md: missing ### $item" >> /tmp/toto-doc-missing.txt
    done
done
if [ -s /tmp/toto-doc-missing.txt ]; then
    cat /tmp/toto-doc-missing.txt
    exit 1
fi
(cd stdlib && ../target/debug/toto test)
# wordfreq sample app (Phase 5, Stage 5): project tests green, and
# the built executable matches the interpreter byte-for-byte.
(cd apps/wordfreq && ../../target/debug/toto test)
(cd apps/wordfreq && ../../target/debug/toto run main.gzv > /tmp/wordfreq_interp.out && ../../target/debug/toto build >/dev/null && rm -f report.json && ./target/wordfreq > /tmp/wordfreq_built.out && cmp /tmp/wordfreq_interp.out /tmp/wordfreq_built.out && rm -f report.json)
