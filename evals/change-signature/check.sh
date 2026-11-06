#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
grep -rqw f64 src && fail "f64 still in src/"
restore_tests
run_tests
