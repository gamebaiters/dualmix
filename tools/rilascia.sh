#!/usr/bin/env bash
# DualMix - release from claudenest: builds on dev-linux, keeps the files among the artifacts and puts them on the
# shared disk (Nextcloud), in the folder Marco installs from.
#
#   tools/rilascia.sh ["what is new, one line"]
#
# Before: dev-linux (VM 110) switched on (claude-vm start 110) - it has the .NET 10 SDK in /opt/dotnet, NSIS and zip.
# After:  /srv/claude/artifacts/DualMix/<version>/   and   disco: condivisi/DualMix/
# The version is <Version> in DualMix/DualMix.csproj: raise it before a new release.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
build_host="${DUALMIX_BUILD_HOST:-root@10.77.0.10}"
remote_dir="/root/DualMix"
notes="${1:-}"
version="$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$root/DualMix/DualMix.csproj" | head -1)"
artifacts="/srv/claude/artifacts/DualMix/$version"
share="condivisi/DualMix"

echo "== DualMix $version: build on $build_host"
rsync -a --delete --exclude .git --exclude bin --exclude obj --exclude out -e "ssh -o BatchMode=yes" "$root/" "$build_host:$remote_dir/"
ssh -o BatchMode=yes "$build_host" "cd $remote_dir && tools/build-release.sh --notes $(printf '%q' "$notes")"

echo "== artifacts: $artifacts"
mkdir -p "$artifacts"
rsync -a --delete -e "ssh -o BatchMode=yes" "$build_host:$remote_dir/out/release/" "$artifacts/"
(cd "$artifacts" && sha256sum -c --quiet SHA256SUMS.txt)

echo "== shared disk: $share"
disco mkdir "$share"
# version.json goes last: a DualMix that reads the folder never finds a version whose package is not there yet
for f in "$artifacts"/*.zip "$artifacts"/*.exe "$artifacts/SHA256SUMS.txt" "$root/packaging/LEGGIMI.txt" "$artifacts/version.json"; do
  disco put "$f" "$share/" --sovrascrivi --verifica
done
disco ls "$share" -l
echo "== done: DualMix $version"
