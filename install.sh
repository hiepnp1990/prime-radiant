#!/bin/sh
# Installs the prime-radiant binary from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/hiepnp1990/prime-radiant/main/install.sh | sh
#
# PRIME_RADIANT_VERSION  Release tag to install, such as v0.1.0. Default: the latest release.
# PREFIX                 Installs to $PREFIX/bin. Default: ~/.local.
# PRIME_RADIANT_BASE_URL URL of a folder that holds the release asset and SHA256SUMS.
#                        Overrides PRIME_RADIANT_VERSION.
set -eu

repo=hiepnp1990/prime-radiant

say() { printf '%s\n' "$*"; }
die() {
	printf 'install.sh: %s\n' "$*" >&2
	exit 1
}

platform="$(uname -s) $(uname -m)"
case $platform in
"Linux x86_64") asset=prime-radiant-linux-x86_64 ;;
"Darwin arm64") asset=prime-radiant-macos-arm64 ;;
*) die "no prebuilt binary for $platform. Build from source instead: cargo install --git https://github.com/$repo --locked --root ${PREFIX:-~/.local}" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required"
if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$1"; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$1"; }
else
	die "sha256sum or shasum is required to check the download"
fi

version=${PRIME_RADIANT_VERSION:-latest}
case $version in
latest) base_url=https://github.com/$repo/releases/latest/download ;;
*) base_url=https://github.com/$repo/releases/download/v${version#v} ;;
esac
base_url=${PRIME_RADIANT_BASE_URL:-$base_url}

bindir=${PREFIX:-$HOME/.local}/bin
staged=$bindir/.prime-radiant.$$
tmp=$(mktemp -d "${TMPDIR:-/tmp}/prime-radiant.XXXXXX")
trap 'rm -rf "$tmp" "$staged"' EXIT

say "Downloading $asset from $base_url"
curl -fsSL "$base_url/$asset" -o "$tmp/$asset" || die "could not download $base_url/$asset"
curl -fsSL "$base_url/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "could not download $base_url/SHA256SUMS"

expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1; exit }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS has no checksum for $asset. Nothing was installed."
actual=$(sha256 "$tmp/$asset" | awk '{ print $1 }')
[ "$actual" = "$expected" ] || die "checksum mismatch for $asset: SHA256SUMS says $expected, the download is $actual. Nothing was installed."

# Stage the binary in the same folder, then rename it, so a running prime-radiant keeps its old file.
mkdir -p "$bindir"
cp "$tmp/$asset" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$bindir/prime-radiant"

say "Installed prime-radiant to $bindir/prime-radiant"
case ":$PATH:" in
*":$bindir:"*) say "$bindir is on your PATH." ;;
*)
	say "$bindir is not on your PATH. To run prime-radiant by name, add this line to your shell's startup file:"
	say "  export PATH=\"$bindir:\$PATH\""
	;;
esac
