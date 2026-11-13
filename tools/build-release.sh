#!/usr/bin/env bash
# DualMix - builds a release: the package (zip), the installer (exe) and the version document (version.json).
#
#   tools/build-release.sh [--base-url <address of the folder the package is downloaded from>] [--notes "<text>"]
#
# Runs on Linux (and in GitHub Actions): needs the .NET 10 SDK, zip, and NSIS (makensis) for the installer.
# The version is the <Version> of DualMix/DualMix.csproj. Output in out/release/:
#
#   DualMix-<v>-win-x64.zip     the package: "Install DualMix.cmd", setup\, app\   (what the automatic update downloads)
#   DualMix-Setup-<v>.exe       the same package as a double-click installer
#   Remove-old-DualMix.zip      the script that removes a DualMix installed the old way, on its own
#   version.json                what a running DualMix reads to know there is a new version
#   SHA256SUMS.txt
#
# With --base-url the "url" in version.json is absolute (GitHub: https://github.com/<owner>/<repo>/releases/download/v<v>);
# without it, it is just the file name: version.json and the zip then live in the same folder.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
base_url=""
notes=""
while [ $# -gt 0 ]; do
  case "$1" in
    --base-url) base_url="${2%/}"; shift 2 ;;
    --notes) notes="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

version="$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$root/DualMix/DualMix.csproj" | head -1)"
[ -n "$version" ] || { echo "version not found in DualMix.csproj" >&2; exit 1; }
out="$root/out/release"
stage="$root/out/stage"
zip_name="DualMix-$version-win-x64.zip"
exe_name="DualMix-Setup-$version.exe"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1

echo "== DualMix $version"
rm -rf "$out" "$stage"
mkdir -p "$out" "$stage/app" "$stage/setup"

echo "== publish (Release, win-x64, ReadyToRun)"
dotnet publish "$root/DualMix/DualMix.csproj" -c Release -r win-x64 --self-contained false \
  -o "$stage/app" -p:DebugType=none -nologo -v q
[ -f "$stage/app/DualMix.dll" ] || { echo "publish produced no DualMix.dll" >&2; exit 1; }
cp "$root/DualMix/Assets/dualmix.ico" "$stage/app/dualmix.ico"

cp "$root/packaging/setup/"*.ps1 "$stage/setup/"
cp "$root/packaging/"*.cmd "$stage/"
# Windows PowerShell 5.1 and cmd want CRLF line ends
sed -i 's/\r$//; s/$/\r/' "$stage/setup/"*.ps1 "$stage/"*.cmd

echo "== package"
(cd "$stage" && zip -q -r -X "$out/$zip_name" .)
old="$root/out/rimuovi-vecchio"
rm -rf "$old"; mkdir -p "$old/setup"
cp "$stage/setup/rimuovi-vecchio.ps1" "$old/setup/"
cp "$stage/Remove old DualMix.cmd" "$old/"
(cd "$old" && zip -q -r -X "$out/Remove-old-DualMix.zip" .)

if command -v makensis >/dev/null; then
  echo "== installer"
  makensis -V2 -DVERSION="$version" -DSTAGE="$stage" -DOUTFILE="$out/$exe_name" "$root/packaging/installer.nsi"
else
  echo "== installer skipped: makensis not found" >&2
fi

sha="$(sha256sum "$out/$zip_name" | cut -d' ' -f1)"
url="$zip_name"
[ -n "$base_url" ] && url="$base_url/$zip_name"
python3 - "$out/version.json" "$version" "$url" "$sha" "$notes" <<'PY'
import json, sys
path, version, url, sha, notes = sys.argv[1:6]
doc = {"product": "DualMix", "version": version, "url": url, "sha256": sha}
if notes:
    doc["notes"] = notes
with open(path, "w", encoding="utf-8", newline="\n") as f:
    json.dump(doc, f, ensure_ascii=False, indent=2)
    f.write("\n")
PY
(cd "$out" && sha256sum -- *.zip *.exe 2>/dev/null > SHA256SUMS.txt)

echo "== done: $out"
ls -l "$out"
