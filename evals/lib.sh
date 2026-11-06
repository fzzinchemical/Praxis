# Sourced by every evals/<task>/check.sh. Usage: check.sh [workdir], where workdir is the
# agent's edited copy of repo/ (default: the untouched fixture). Works on a throwaway copy,
# so a check never changes the workdir and never sees the harness's own Cargo workspace.
set -euo pipefail

task_dir=$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)
pristine=$task_dir/repo
work=$(cd "${1:-$pristine}" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cp -a "$work/." "$scratch/"
cd "$scratch"
rm -rf target .git
export CARGO_TARGET_DIR=$scratch/.check-target

fail() {
    echo "check: $*" >&2
    exit 1
}

# Discards everything the agent could use to change what gets tested, then installs the
# fixture's tests plus hidden/, which lives outside the agent's writable path (SPEC §14).
restore_tests() {
    rm -rf tests build.rs .cargo rust-toolchain rust-toolchain.toml Cargo.lock
    cp "$pristine/Cargo.toml" Cargo.toml
    if [[ -d $pristine/tests ]]; then cp -a "$pristine/tests" tests; else mkdir tests; fi
    cp "$task_dir"/hidden/*.rs tests/
}

# Exit status of the integration tests, with their output kept short.
cargo_tests() {
    timeout 300 cargo test -q --offline --test '*' >test.log 2>&1
}

run_tests() {
    cargo_tests || {
        tail -n 15 test.log >&2
        fail "tests failed"
    }
}
