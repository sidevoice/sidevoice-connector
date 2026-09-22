#!/bin/sh
# What changed between two tags, from the commit subjects: this repository writes conventional
# subjects with the reason in the body, so the subjects alone read as release notes. Grouped by
# kind, each line linking its commit; `chore: release …` commits are the version bump itself and
# are left out. Usage: scripts/release-notes.sh <previous-tag-or-commit> <tag> [repository]
set -eu
from=$1; to=$2; repo=${3:-$(git config --get remote.origin.url | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')}
section() {
  pattern=$1; title=$2
  lines=$(git log --no-merges --format='%h%x09%s' "$from..$to" | grep -E "^[0-9a-f]+	$pattern" | grep -vE '	chore: release ' || true)
  [ -n "$lines" ] || return 0
  printf '### %s\n\n' "$title"
  printf '%s\n' "$lines" | while IFS='	' read -r sha subject; do
    text=$(printf '%s' "$subject" | sed -E 's/^[a-z]+(\([^)]*\))?!?: //')
    printf -- '- %s ([%s](https://github.com/%s/commit/%s))\n' "$text" "$sha" "$repo" "$sha"
  done
  printf '\n'
}
printf '## What changed\n\n'
section 'feat(\(|:)' 'Features'
section 'fix(\(|:)' 'Fixes'
section 'docs(\(|:)' 'Documentation'
section '(refactor|perf|test|ci|build|chore)(\(|:)' 'Maintenance'
