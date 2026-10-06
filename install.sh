#!/bin/sh
# Install a checksummed GitHub release without Rust or a package manager.
set -eu

repository=tiaanduplessis/wtm
install_dir=${WTM_INSTALL_DIR:-"$HOME/.local/bin"}
version=
temporary=
staging=

fail() {
    printf 'wtm: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "$staging" ]; then rm -f "$staging"; fi
    if [ -n "$temporary" ]; then rm -rf "$temporary"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || fail '--version needs a tag such as v0.1.0'
            version=$2
            shift 2
            ;;
        --dir)
            [ "$#" -ge 2 ] || fail '--dir needs an installation directory'
            install_dir=$2
            shift 2
            ;;
        --help|-h)
            printf '%s\n' 'Usage: sh install.sh [--version v0.1.0] [--dir DIRECTORY]' \
                'Defaults: latest stable GitHub release, ~/.local/bin.' \
                'Platforms: macOS and Linux, x86_64 and arm64.'
            exit 0
            ;;
        *) fail "unknown option: $1" ;;
    esac
done

for required in curl tar awk mktemp git; do
    command -v "$required" >/dev/null 2>&1 || fail "$required is required"
done
git_version=$(git --version | awk '{print $3}')
printf '%s\n' "$git_version" | awk -F. '{ exit !($1 > 2 || ($1 == 2 && $2 >= 36)) }' \
    || fail 'Git 2.36 or newer is required'

system=$(uname -s)
architecture=$(uname -m)
case "$system/$architecture" in
    Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
    Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-musl ;;
    *) fail "no release binary for $system/$architecture; install from source instead" ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
    checksum_tool=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    checksum_tool=shasum
else
    fail 'sha256sum or shasum is required to verify the download'
fi

download() {
    curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' \
        --tlsv1.2 --connect-timeout 10 --max-time 120 "$@"
}

if [ -z "$version" ]; then
    release_url=$(download --output /dev/null --write-out '%{url_effective}' \
        "https://github.com/$repository/releases/latest") \
        || fail 'could not find the latest release; use source installation if no release exists yet'
    case "$release_url" in
        "https://github.com/$repository/releases/tag/"*) version=${release_url##*/} ;;
        *) fail 'GitHub did not return a release tag' ;;
    esac
fi
printf '%s\n' "$version" | LC_ALL=C grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' \
    || fail 'release version must be a stable tag such as v0.1.0'

temporary=$(mktemp -d "${TMPDIR:-/tmp}/wtm-install.XXXXXX")
asset="wtm-$version-$target.tar.gz"
base="https://github.com/$repository/releases/download/$version"
download --output "$temporary/$asset" "$base/$asset" || fail 'could not download the release archive'
download --output "$temporary/SHA256SUMS" "$base/SHA256SUMS" || fail 'could not download release checksums'
expected=$(awk -v asset="$asset" '$2 == asset { print $1; count++ } END { if (count != 1) exit 1 }' \
    "$temporary/SHA256SUMS") || fail 'release checksum is missing or ambiguous'
case "$expected" in ''|*[!0-9a-f]*) fail 'release checksum is malformed' ;; esac
[ "${#expected}" -eq 64 ] || fail 'release checksum is malformed'
if [ "$checksum_tool" = sha256sum ]; then
    actual=$(sha256sum "$temporary/$asset" | awk '{print $1}')
else
    actual=$(shasum -a 256 "$temporary/$asset" | awk '{print $1}')
fi
[ "$actual" = "$expected" ] || fail 'checksum mismatch; the installed binary was not changed'

# Stream just the binary out of the archive; never extract archive-supplied paths.
tar -xOzf "$temporary/$asset" wtm > "$temporary/wtm" || fail 'release archive has no wtm binary'
[ -s "$temporary/wtm" ] || fail 'release binary is empty'
chmod 755 "$temporary/wtm"
reported=$("$temporary/wtm" --version) || fail 'release binary cannot run on this machine'
[ "$reported" = "worktree-manager ${version#v}" ] || fail 'release binary version does not match the tag'

[ -n "$install_dir" ] || fail 'installation directory must not be empty'
mkdir -p "$install_dir"
[ ! -d "$install_dir/wtm" ] || fail 'installation target is a directory'
staging=$(mktemp "$install_dir/.wtm-install.XXXXXX")
cp "$temporary/wtm" "$staging"
chmod 755 "$staging"
mv -f "$staging" "$install_dir/wtm"
staging=
printf 'Installed wtm %s in %s.\n' "${version#v}" "$install_dir"
printf 'Add the installation directory to PATH if wtm is not found.\n'
