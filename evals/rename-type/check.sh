#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
grep -rqF UserRecord src README.md && fail "UserRecord still present"
restore_tests
run_tests
