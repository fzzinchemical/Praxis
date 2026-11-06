#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
[[ -f src/catalog.rs ]] || fail "src/catalog.rs is gone"
changed=$(diff "$pristine/src/catalog.rs" src/catalog.rs | grep -c '^[<>]' || true)
((changed <= 8)) || fail "$changed lines changed in src/catalog.rs; expected a small edit"
restore_tests
run_tests
