#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
# The tests are the deliverable, so judge them by which faulty versions they catch.
rm -rf src build.rs .cargo Cargo.lock
cp -a "$pristine/src" src
cp "$pristine/Cargo.toml" Cargo.toml
compgen -G 'tests/*.rs' >/dev/null || fail "no tests in tests/"
run_tests
for mutant in "$task_dir"/hidden/mutant_*.rs; do
    cp "$mutant" src/lib.rs
    if cargo_tests; then fail "tests did not catch $(basename "$mutant" .rs)"; fi
    if grep -q 'could not compile' test.log; then fail "tests do not compile against $(basename "$mutant" .rs)"; fi
done
