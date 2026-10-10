#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version=$(awk -F '"' '/^version[[:space:]]*=/ { print $2; exit }' herdr-plugin.toml)
repo=jaredlyon/herdr-respan-adapter
case "$(uname -s)" in
  Darwin)
    arch=$(uname -m)
    if [ "$arch" = arm64 ] || [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
      target=aarch64-apple-darwin
    else target=x86_64-apple-darwin; fi ;;
  Linux)
    case "$(uname -m)" in
      aarch64|arm64) target=aarch64-unknown-linux-musl ;;
      x86_64|amd64) target=x86_64-unknown-linux-musl ;;
      *) target=unsupported ;;
    esac ;;
  *) target=unsupported ;;
esac
mkdir -p bin
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
asset=herdr-respan-$target
url=https://github.com/$repo/releases/download/v$version
fetch() { curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 "$1" -o "$2"; }
if [ "$target" != unsupported ] && command -v curl >/dev/null 2>&1 && fetch "$url/$asset" "$tmp/$asset" && fetch "$url/SHA256SUMS" "$tmp/SHA256SUMS"; then
  expected=$(awk -v name="$asset" '$2 == name {print $1}' "$tmp/SHA256SUMS")
  [ "${#expected}" = 64 ] || { echo 'Missing or invalid release checksum' >&2; exit 1; }
  if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/$asset");
  elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 "$tmp/$asset");
  else echo 'SHA256 verification needs sha256sum or shasum' >&2; exit 1; fi
  actual=${actual%% *}
  [ "$actual" = "$expected" ] || { echo 'Release SHA256 mismatch; refusing executable' >&2; exit 1; }
  cp "$tmp/$asset" bin/herdr-respan.exe.new
else
  echo 'Prebuilt release unavailable; building from source with Cargo.' >&2
  if command -v cargo >/dev/null 2>&1; then cargo=cargo
  elif [ -x "$HOME/.cargo/bin/cargo" ]; then cargo=$HOME/.cargo/bin/cargo
  else echo 'Install Rust/Cargo or use a supported prebuilt release.' >&2; exit 1; fi
  "$cargo" build --release --locked
  cp target/release/herdr-respan bin/herdr-respan.exe.new
fi
chmod 755 bin/herdr-respan.exe.new
mv -f bin/herdr-respan.exe.new bin/herdr-respan.exe
