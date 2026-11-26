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
