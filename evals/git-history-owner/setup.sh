#!/usr/bin/env bash
# Builds the fixture's git history in the copy, since a nested .git can't be committed here.
# Fixed dates keep the commits the same on every run.
set -euo pipefail
cd "$1"
git init -q -b main
export GIT_AUTHOR_NAME=eval GIT_AUTHOR_EMAIL=eval@example.invalid
export GIT_COMMITTER_NAME=eval GIT_COMMITTER_EMAIL=eval@example.invalid
commit() {
    git add -A
    GIT_AUTHOR_DATE="$1" GIT_COMMITTER_DATE="$1" git commit -q -m "$2"
}
mkdir -p src
printf 'pub fn parse_config(text: &str) -> usize {\n    text.len() + 3\n}\n' >src/config.rs
commit "2026-01-05T10:00:00Z" "Add parse_config"
printf 'pub fn parse_config(text: &str) -> usize {\n    text.len() + 5\n}\n' >src/config.rs
commit "2026-02-10T10:00:00Z" "Tighten retry budget"
printf '# Config demo\n\nDocs only.\n' >README.md
commit "2026-03-01T10:00:00Z" "Expand the README"
