#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
restore_tests
grep -qE '^418 Teapot$' data/codes.txt || fail "data/codes.txt was not updated"
cp src/codes.rs codes.before
cp "$pristine/gen.sh" gen.sh
bash gen.sh
cmp -s src/codes.rs codes.before || fail "src/codes.rs does not match gen.sh output"
run_tests
