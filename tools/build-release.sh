#!/usr/bin/env bash
# DualMix - builds a release: the package (zip), the installer (exe) and the version document (version.json).
#
#   tools/build-release.sh [--exe <path/to/DualMix.exe>] [--base-url <address>] [--notes "<text>"]
#
# Uses the native Rust binary (`--exe`, or `dualmix-rs/target/release/DualMix.exe` when present), falling back to
# `dotnet publish` only when `--dotnet` is passed. Output in out/release/.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
base_url=""
notes=""
exe_path=""
force_dotnet=0
while [ $# -gt 0 ]; do
  case "$1" in
    --base-url) base_url="${2%/}"; shift 2 ;;
    --notes) notes="$2"; shift 2 ;;
    --exe) exe_path="$2"; shift 2 ;;
    --dotnet) force_dotnet=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ "$force_dotnet" -eq 0 ] && [ -z "$exe_path" ] && [ -f "$root/dualmix-rs/target/release/DualMix.exe" ]; then
  exe_path="$root/dualmix-rs/target/release/DualMix.exe"
fi

if [ -n "$exe_path" ] || [ "$force_dotnet" -eq 0 ]; then
  version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/dualmix-rs/Cargo.toml" | head -1)"
else
  version="$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$root/DualMix/DualMix.csproj" | head -1)"
fi
[ -n "$version" ] || { echo "version not found" >&2; exit 1; }
out="$root/out/release"
stage="$root/out/stage"
zip_name="DualMix-$version-win-x64.zip"
exe_name="DualMix-Setup-$version.exe"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1

echo "== DualMix $version"
rm -rf "$out" "$stage"
mkdir -p "$out" "$stage/app" "$stage/setup"

if [ "$force_dotnet" -eq 0 ]; then
  if [ -z "$exe_path" ] || [ ! -f "$exe_path" ]; then
    echo "== build native Rust binary (Release, win-x64)"
    cargo build --release --manifest-path "$root/dualmix-rs/Cargo.toml"
    exe_path="$root/dualmix-rs/target/release/DualMix.exe"
  fi
  [ -f "$exe_path" ] || { echo "native binary not found: $exe_path" >&2; exit 1; }
  echo "== stage native binary: $exe_path"
  cp "$exe_path" "$stage/app/DualMix.exe"
else
  echo "== publish (Release, win-x64, ReadyToRun)"
  dotnet publish "$root/DualMix/DualMix.csproj" -c Release -r win-x64 --self-contained false \
    -o "$stage/app" -p:DebugType=none -nologo -v q
  [ -f "$stage/app/DualMix.dll" ] || { echo "publish produced no DualMix.dll" >&2; exit 1; }
fi
cp "$root/DualMix/Assets/dualmix.ico" "$stage/app/dualmix.ico"

cp "$root/packaging/setup/"*.ps1 "$stage/setup/"
cp "$root/packaging/"*.cmd "$stage/"
# Windows PowerShell 5.1 and cmd want CRLF line ends
sed -i 's/\r$//; s/$/\r/' "$stage/setup/"*.ps1 "$stage/"*.cmd

make_zip() {
  local src_dir="$1" dest_zip="$2"
  if command -v zip >/dev/null 2>&1; then
    (cd "$src_dir" && zip -q -r -X "$dest_zip" .)
  else
    python3 - "$src_dir" "$dest_zip" <<'PY'
import os, sys, zipfile
src, dst = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(dst, "w", zipfile.ZIP_DEFLATED) as zf:
    for r, _, files in os.walk(src):
        for fn in sorted(files):
            p = os.path.join(r, fn)
            zf.write(p, os.path.relpath(p, src))
PY
  fi
}

echo "== package"
make_zip "$stage" "$out/$zip_name"
old="$root/out/rimuovi-vecchio"
rm -rf "$old"; mkdir -p "$old/setup"
cp "$stage/setup/rimuovi-vecchio.ps1" "$old/setup/"
cp "$stage/Remove old DualMix.cmd" "$old/"
make_zip "$old" "$out/Remove-old-DualMix.zip"

if ! command -v makensis >/dev/null && [ -x "/c/Program Files (x86)/NSIS/makensis.exe" ]; then
  PATH="$PATH:/c/Program Files (x86)/NSIS"
fi
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
(
  cd "$out"
  shopt -s nullglob
  files=( *.zip *.exe )
  [ ${#files[@]} -gt 0 ] && sha256sum -- "${files[@]}" > SHA256SUMS.txt
)

echo "== done: $out"
ls -l "$out"
