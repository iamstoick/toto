#!/bin/sh
# Repo gate: formatting, lints, tests. No CI config exists yet
# (repo host unknown), so this script is the gate.
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
cargo test --workspace
