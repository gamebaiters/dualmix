#!/usr/bin/env bash
# DualMix - release from claudenest: packages the native Rust DualMix.exe (or builds on dev-win),
# keeps artifacts in /srv/claude/artifacts/DualMix/<version>/ and uploads to Nextcloud shared disk.
#
#   tools/rilascia.sh [--exe /path/to/DualMix.exe] ["what is new, one line"]
#
# Before: DualMix.exe built on dev-win (C:\dm-rs\dualmix-rs\target\release\DualMix.exe) or passed via --exe.
# After:  /srv/claude/artifacts/DualMix/<version>/   and   disco: condivisi/DualMix/
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
exe_path=""
if [ "${1:-}" = "--exe" ]; then
  exe_path="${2:-}"
  shift 2
fi
notes="${1:-}"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/dualmix-rs/Cargo.toml" | head -1)"
artifacts="/srv/claude/artifacts/DualMix/$version"
share="condivisi/DualMix"

echo "== DualMix $version: package release"
if [ -n "$exe_path" ]; then
  "$root/tools/build-release.sh" --exe "$exe_path" --notes "$notes"
else
  "$root/tools/build-release.sh" --notes "$notes"
fi

echo "== artifacts: $artifacts"
mkdir -p "$artifacts"
rsync -a --delete "$root/out/release/" "$artifacts/"
(cd "$artifacts" && sha256sum -c --quiet SHA256SUMS.txt)

echo "== shared disk: $share"
disco mkdir "$share"
# version.json goes last: a DualMix that reads the folder never finds a version whose package is not there yet
shopt -s nullglob
for f in "$artifacts"/*.zip "$artifacts"/*.exe "$artifacts/SHA256SUMS.txt" "$root/packaging/README.txt" "$root/packaging/LEGGIMI.txt" "$artifacts/version.json"; do
  disco put "$f" "$share/" --sovrascrivi --verifica
done
disco ls "$share" -l
echo "== done: DualMix $version"
