#!/usr/bin/env bash
# Downloads the sqllogictest files listed in sqllogictest/manifest.txt from
# sqlite.org at the pinned check-in, then verifies every SHA-256.
# Files land in sqllogictest/test/, which git ignores.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/sqllogictest/manifest.txt"
dest="$root/sqllogictest/test"

header() { sed -n "s/^# $1: *//p" "$manifest"; }
source_url="$(header source)"
checkin="$(header checkin)"

grep -v '^#' "$manifest" | while read -r sha path; do
  [ -z "$path" ] && continue
  target="$dest/$path"
  if [ -f "$target" ] && echo "$sha  $target" | shasum -a 256 -c --status; then
    continue
  fi
  mkdir -p "$(dirname "$target")"
  echo "fetching $path"
  curl -sSfL "$source_url/raw/test/$path?ci=$checkin" -o "$target"
done

cd "$dest"
grep -v '^#' "$manifest" | shasum -a 256 -c --quiet
echo "all files match the manifest"
