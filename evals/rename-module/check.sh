#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
[[ ! -e src/util.rs && ! -d src/util ]] || fail "old module file still exists"
grep -rqw util src && fail "util still referenced"
restore_tests
run_tests
