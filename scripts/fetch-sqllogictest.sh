#!/usr/bin/env bash
# Downloads the sqllogictest files a manifest lists from sqlite.org at the
# pinned check-in, then verifies every SHA-256.
#
#   scripts/fetch-sqllogictest.sh          # the 29-file sample, about 26 MB
#   scripts/fetch-sqllogictest.sh --full   # the full suite, 622 files, about 1 GB
#
# Files land in sqllogictest/test/, which git ignores. On first use it is made
# a symlink to test.nosync so iCloud Drive does not sync the corpus. The sample
# is a subset of the full suite, so both share the directory.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/sqllogictest/manifest.txt"
case "${1:-}" in
  "") ;;
  --full) manifest="$root/sqllogictest/manifest-full.txt" ;;
  --setup-only) ;;
  *) echo "usage: scripts/fetch-sqllogictest.sh [--full]" >&2; exit 2 ;;
esac

dest="$root/sqllogictest/test"
if [ ! -e "$dest" ]; then
  mkdir -p "$dest.nosync"
  ln -s test.nosync "$dest"
fi
[ "${1:-}" = "--setup-only" ] && exit 0

header() { sed -n "s/^# $1: *//p" "$manifest"; }
source_url="$(header source)"
checkin="$(header checkin)"
export source_url checkin dest

fetch() {
  local sha="$1" path="$2" target="$dest/$2"
  if [ -f "$target" ] && echo "$sha  $target" | shasum -a 256 -c --status; then
    return
  fi
  mkdir -p "$(dirname "$target")"
  echo "fetching $path"
  curl -sSfL --retry 3 "$source_url/raw/test/$path?ci=$checkin" -o "$target.part"
  mv "$target.part" "$target"
}
export -f fetch
# Four downloads at a time is quick and polite to sqlite.org.
grep -v '^#' "$manifest" | xargs -P 4 -n 2 bash -c 'fetch "$0" "$1"'

cd "$dest"
grep -v '^#' "$manifest" | shasum -a 256 -c --quiet
echo "all $(grep -vc '^#' "$manifest") files match $(basename "$manifest")"
