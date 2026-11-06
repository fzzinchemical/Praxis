#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
sed -n '/^## Unreleased/,/^## [0-9]/p' CHANGELOG.md | grep -qE '^[-*] .*parse_port' || fail "no CHANGELOG entry under Unreleased"
grep -B1 -E 'pub fn parse_port' src/lib.rs | grep -q '///' || fail "parse_port has no doc comment"
grep -rqE '\.unwrap\(|\.expect\(|panic!' src && fail "src/ uses unwrap, expect or panic"
restore_tests
run_tests
