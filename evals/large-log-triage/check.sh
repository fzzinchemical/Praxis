#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
cmp -s "$pristine/logs/app.log" logs/app.log || fail "the log was changed"
expected='# Service settings
port = 8443
log_level = debug
max_connections = 64
timeout_secs = 30'
[[ $(grep -v '^$' config/app.conf) == "$expected" ]] || fail "config/app.conf is not fixed"
