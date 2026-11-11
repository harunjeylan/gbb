#!/bin/sh
# Install a prebuilt gbb binary from GitHub releases.
#
#   curl -fsSL https://github.com/harunjeylan/gbb/releases/latest/download/install.sh | sh
#
# Override the install directory or version with:
#   GBB_INSTALL_DIR="$HOME/bin" GBB_VERSION=v0.1.0 curl ... | sh
set -eu

repo="${GBB_REPO:-harunjeylan/gbb}"
version="${GBB_VERSION:-latest}"
dest="${GBB_INSTALL_DIR:-${HOME}/.local/bin}"

fail() {
	printf 'gbb-install: %s\n' "$*" >&2
	exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

os="$(uname -s)"
[ "$os" = "Linux" ] || fail "prebuilt binaries are Linux-only; use: cargo install --git https://github.com/${repo}"

case "$(uname -m)" in
x86_64 | amd64) triple="x86_64-unknown-linux-gnu" ;;
aarch64 | arm64) triple="aarch64-unknown-linux-gnu" ;;
*) fail "no prebuilt binary for architecture $(uname -m)" ;;
esac

asset="gbb-${triple}.tar.gz"
if [ "$version" = "latest" ]; then
	base="https://github.com/${repo}/releases/latest/download"
else
	base="https://github.com/${repo}/releases/download/${version}"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

printf 'Downloading %s/%s\n' "$base" "$asset"
curl --proto '=https' --tlsv1.2 -fsSL "${base}/${asset}" -o "${tmp}/${asset}" ||
	fail "download failed (no prebuilt ${triple} asset for ${version})"

# Releases publish SHA256SUMS; verify when it is available.
if command -v sha256sum >/dev/null 2>&1 &&
	curl --proto '=https' --tlsv1.2 -fsSL "${base}/SHA256SUMS" -o "${tmp}/SHA256SUMS" 2>/dev/null; then
	expected="$(awk -v a="$asset" '$2 == a { print $1 }' "${tmp}/SHA256SUMS")"
	actual="$(sha256sum "${tmp}/${asset}" | awk '{ print $1 }')"
	[ -n "$expected" ] && [ "$expected" = "$actual" ] || fail "checksum mismatch for ${asset}"
fi

tar -xzf "${tmp}/${asset}" -C "$tmp"
binary="$(find "${tmp}" -type f -name gbb -perm -u+x | head -n 1)"
[ -n "$binary" ] || fail "release archive did not contain a gbb binary"

mkdir -p "$dest"
install -m 0755 "$binary" "${dest}/gbb"
"${dest}/gbb" version >/dev/null || fail "installed binary does not run on this machine"

printf 'Installed gbb to %s\n' "${dest}/gbb"
case ":${PATH}:" in
*":${dest}:"*) ;;
*) printf 'Add it to PATH:\n  export PATH="%s:$PATH"\n' "$dest" ;;
esac
