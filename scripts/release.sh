#!/usr/bin/env bash
# Bump the workspace version, commit, and tag vX.Y.Z (which triggers release.yml).
#
#   scripts/release.sh            # patch bump (0.1.0 -> 0.1.1)
#   scripts/release.sh 0.2.0      # explicit version
set -euo pipefail
cd "$(dirname "$0")/.."

cur="$(grep -m1 '^version = ' server/Cargo.toml | cut -d'"' -f2)"
if [ $# -ge 1 ]; then
  new="$1"
else
  IFS=. read -r a b c <<<"$cur"
  new="$a.$b.$((c + 1))"
fi

echo "version: $cur -> $new"
perl -0pi -e "s/^version = \"[^\"]+\"/version = \"$new\"/m" server/Cargo.toml
git add server/Cargo.toml
git commit -m "release v$new"
git tag "v$new"
echo "tagged v$new — push with:  git push && git push --tags"
