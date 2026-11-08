#!/bin/sh
set -eu

repo=IniZio/crabbox-cloudhypervisor
bin=crabbox-cloudhypervisor
dir=${CRABBOX_CLOUDHYPERVISOR_INSTALL_DIR:-$HOME/.local/bin}
version=${CRABBOX_CLOUDHYPERVISOR_VERSION:-}

fail() {
  echo "install.sh: $*" >&2
  exit 1
}

for tool in curl tar sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done

[ "$(uname -s)" = Linux ] || fail "unsupported OS $(uname -s); cloud-hypervisor needs Linux with KVM"
case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "unsupported CPU $(uname -m); releases cover x86_64 and aarch64" ;;
esac
asset=$bin-$arch-unknown-linux-musl.tar.gz

if [ -z "$version" ]; then
  url=https://api.github.com/repos/$repo/releases/latest
  body=$(curl -fsSL "$url") || fail "could not look up the latest release at $url"
  version=$(printf '%s\n' "$body" | sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$version" ] || fail "no release tag found at $url"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
base=https://github.com/$repo/releases/download/$version
curl -fsSL -o "$tmp/$asset" "$base/$asset" || fail "download failed: $base/$asset"
curl -fsSL -o "$tmp/$asset.sha256" "$base/$asset.sha256" || fail "download failed: $base/$asset.sha256"
want=$(cut -d ' ' -f 1 "$tmp/$asset.sha256")
got=$(sha256sum "$tmp/$asset" | cut -d ' ' -f 1)
[ "$want" = "$got" ] || fail "checksum mismatch for $asset: expected $want, got $got"
echo "sha256 ok  $got  $asset"

tar -xzf "$tmp/$asset" -C "$tmp" "$bin" || fail "$asset does not contain $bin"
mkdir -p "$dir"
chmod 755 "$tmp/$bin"
mv -f "$tmp/$bin" "$dir/$bin"
echo "installed $dir/$bin ($version)"

case ":$PATH:" in
  *":$dir:"*) echo "next: $bin install && $bin doctor" ;;
  *) echo "next: add $dir to PATH, then run $bin install && $bin doctor" ;;
esac
