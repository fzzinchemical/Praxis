#!/usr/bin/env bash
source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
[[ $(cat ANSWER.txt) == "Tighten retry budget" ]] || fail "ANSWER.txt is not the subject of the commit that last changed parse_config"
