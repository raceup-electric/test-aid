#!/bin/sh
# Install taid from a verified GitHub release or an existing local binary.
set -eu

usage() {
    printf '%s\n' 'Usage: ./install.sh [--tag vX.Y.Z] [--bin PATH] [--dry-run]' \
        '  --bin PATH   install a local binary without network access' \
        '  --tag TAG    install a specific release (default: latest)' \
        '  --dry-run    print the planned actions without changing files' \
        'Running again replaces the installed binary with the selected version.'
}

fail() { printf 'taid installer: %s\n' "$*" >&2; exit 1; }

tag=latest
local_bin=
dry_run=false
while [ "$#" -gt 0 ]; do
    case $1 in
        --tag) [ "$#" -ge 2 ] || fail '--tag needs a value'; tag=$2; shift 2 ;;
        --bin) [ "$#" -ge 2 ] || fail '--bin needs a path'; local_bin=$2; shift 2 ;;
        --dry-run) dry_run=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown argument: $1" ;;
    esac
done
[ "$tag" = latest ] || [ -z "$local_bin" ] || fail '--tag and --bin cannot be combined'
case $tag in *[!A-Za-z0-9._-]*|'') fail 'invalid release tag' ;; esac

os=$(uname -s)
arch=$(uname -m)
case "$os:$arch" in
    Linux:x86_64|Linux:amd64) target=x86_64-unknown-linux-musl ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-musl ;;
    Linux:armv7l|Linux:armv8l) target=armv7-unknown-linux-musleabihf ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Darwin:arm64) target=aarch64-apple-darwin ;;
    MINGW*:x86_64|MSYS*:x86_64|CYGWIN*:x86_64|Windows:x86_64|MINGW*:AMD64|MSYS*:AMD64|CYGWIN*:AMD64|Windows:AMD64)
        target=x86_64-pc-windows-msvc ;;
    MINGW*:arm64|MSYS*:arm64|CYGWIN*:arm64|Windows:arm64|MINGW*:ARM64|MSYS*:ARM64|CYGWIN*:ARM64|Windows:ARM64)
        target=aarch64-pc-windows-msvc ;;
    *) fail "unsupported platform: $os $arch" ;;
esac
case $target in *windows*) ext=.zip; binary=taid.exe ;; *) ext=.tar.gz; binary=taid ;; esac

install_dir=${TAID_INSTALL_DIR:-"$HOME/.local/bin"}
[ -n "$install_dir" ] || fail 'installation directory is empty'
case $install_dir in /*) ;; *) fail 'TAID_INSTALL_DIR must be an absolute path' ;; esac

if [ -n "$local_bin" ]; then
    [ -f "$local_bin" ] || fail "binary not found: $local_bin"
    [ -x "$local_bin" ] || fail "binary is not executable: $local_bin"
    source_bin=$local_bin
else
    release_repo=${TAID_REPO:-raceup-electric/test-aid}
    auth_token=${GH_TOKEN:-}
    if [ -z "$auth_token" ] && command -v gh >/dev/null 2>&1; then
        auth_token=$(gh auth token --hostname github.com 2>/dev/null || true)
    fi
    case $release_repo in
        */*) ;;
        *) fail 'TAID_REPO must be owner/name' ;;
    esac
    case $release_repo in *[!A-Za-z0-9._/-]*|*/*/*) fail 'invalid TAID_REPO' ;; esac
    if [ "$tag" = latest ]; then release_path=latest/download
    else release_path="download/$tag"; fi
    archive="taid-$tag-$target$ext"
    if [ -n "$auth_token" ] && [ "$dry_run" = false ]; then
        command -v jq >/dev/null 2>&1 || fail 'jq is required for private release downloads'
        command -v curl >/dev/null 2>&1 || fail 'curl is required for private release downloads'
        if [ "$tag" = latest ]; then release_api="https://api.github.com/repos/$release_repo/releases/latest"
        else release_api="https://api.github.com/repos/$release_repo/releases/tags/$tag"; fi
        release_json=$(curl -fsSL -H "Authorization: Bearer $auth_token" -H 'Accept: application/vnd.github+json' "$release_api") || fail 'could not read release metadata'
        tag=$(printf '%s' "$release_json" | jq -r '.tag_name // empty')
        case $tag in *[!A-Za-z0-9._-]*|'') fail 'invalid tag in release metadata' ;; esac
        archive="taid-$tag-$target$ext"
        archive_url=$(printf '%s' "$release_json" | jq -r --arg name "$archive" '[.assets[] | select(.name == $name) | .url] | if length == 1 then .[0] else empty end')
        sums_url=$(printf '%s' "$release_json" | jq -r '[.assets[] | select(.name == "SHA256SUMS.txt") | .url] | if length == 1 then .[0] else empty end')
        [ -n "$archive_url" ] && [ -n "$sums_url" ] || fail 'release archive or checksums not found'
    elif [ "$tag" = latest ]; then
        # GitHub's latest/download endpoint needs the real tag in the asset name.
        # Resolve it from the redirect without downloading an archive.
        if [ "$dry_run" = false ]; then
            if command -v curl >/dev/null 2>&1; then
                final_url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$release_repo/releases/latest") || fail 'could not resolve latest release (for a private repo, authenticate gh or set GH_TOKEN)'
            elif command -v wget >/dev/null 2>&1; then
                final_url=$(wget --spider -S "https://github.com/$release_repo/releases/latest" 2>&1 | sed -n 's/.*Location: //p' | tail -n 1) || fail 'could not resolve latest release (for a private repo, authenticate gh or set GH_TOKEN)'
            else fail 'curl or wget is required for release downloads'; fi
            tag=${final_url##*/}
            case $tag in v[0-9]*|[0-9]*) ;; *) fail 'could not determine latest release tag' ;; esac
            release_path="download/$tag"
            archive="taid-$tag-$target$ext"
        else
            archive="taid-<latest-tag>-$target$ext"
        fi
    fi
    if [ -z "$auth_token" ] || [ "$dry_run" = true ]; then
        base_url="https://github.com/$release_repo/releases/$release_path"
        archive_url="$base_url/$archive"
        sums_url="$base_url/SHA256SUMS.txt"
    fi
fi

case ${SHELL##*/} in
    zsh) rc=$HOME/.zshrc ;;
    bash) if [ "$os" = Darwin ]; then rc=$HOME/.bash_profile; else rc=$HOME/.bashrc; fi ;;
    *) rc=$HOME/.profile ;;
esac

if [ "$dry_run" = true ]; then
    printf 'Platform: %s (%s %s)\n' "$target" "$os" "$arch"
    if [ -n "$local_bin" ]; then printf 'Binary: %s\n' "$local_bin"
    else
        printf 'Archive: %s\nChecksums: %s\n' "$archive_url" "$sums_url"
        [ -z "${auth_token:-}" ] || printf 'Authentication: GH_TOKEN via GitHub API\n'
    fi
    printf 'Install/update: %s\nPATH rc: %s (only if needed)\n' "$install_dir/$binary" "$rc"
    exit 0
fi

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/taid-install.XXXXXXXX") || fail 'could not create temporary directory'
stage_bin=
trap 'rm -rf "$tmp_dir"; [ -z "$stage_bin" ] || rm -f "$stage_bin"' EXIT HUP INT TERM

if [ -z "$local_bin" ]; then
    download() {
        if command -v curl >/dev/null 2>&1; then
            if [ -n "${auth_token:-}" ]; then
                curl -fsSL --retry 3 -H "Authorization: Bearer $auth_token" -H 'Accept: application/octet-stream' -o "$2" "$1"
            else curl -fsSL --retry 3 -o "$2" "$1"; fi
        elif command -v wget >/dev/null 2>&1 && [ -z "${auth_token:-}" ]; then
            wget -q -O "$2" "$1"
        else fail 'curl or wget is required for release downloads'; fi
    }
    download "$archive_url" "$tmp_dir/$archive" || fail "download failed: $archive_url"
    download "$sums_url" "$tmp_dir/SHA256SUMS.txt" || fail "download failed: $sums_url"
    expected=$(awk -v name="$archive" '$2 == name || $2 == "*" name { print $1; n++ } END { if (n != 1) exit 1 }' "$tmp_dir/SHA256SUMS.txt") || fail "checksum missing or duplicated for $archive"
    case $expected in *[!0-9a-fA-F]*|'') fail 'invalid SHA-256 checksum' ;; esac
    [ "${#expected}" -eq 64 ] || fail 'invalid SHA-256 checksum length'
    if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp_dir/$archive" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 "$tmp_dir/$archive" | awk '{print $1}')
    elif command -v openssl >/dev/null 2>&1; then actual=$(openssl dgst -sha256 "$tmp_dir/$archive" | awk '{print $NF}')
    else fail 'sha256sum, shasum, or openssl is required to verify downloads'; fi
    [ "$(printf '%s' "$actual" | tr 'A-F' 'a-f')" = "$(printf '%s' "$expected" | tr 'A-F' 'a-f')" ] || fail "SHA-256 mismatch for $archive"
    if [ "$ext" = .zip ]; then
        command -v unzip >/dev/null 2>&1 || fail 'unzip is required for Windows archives'
        unzip -p "$tmp_dir/$archive" "$binary" > "$tmp_dir/$binary" || fail "archive lacks $binary"
    else
        tar -xzf "$tmp_dir/$archive" -C "$tmp_dir" "$binary" || fail "archive lacks $binary"
    fi
    source_bin="$tmp_dir/$binary"
fi

[ ! -d "$install_dir/$binary" ] || fail "installation target is a directory: $install_dir/$binary"
install_action=Installed
if [ -e "$install_dir/$binary" ] || [ -L "$install_dir/$binary" ]; then install_action=Updated; fi
mkdir -p "$install_dir"
stage_bin=$(mktemp "$install_dir/.taid.XXXXXXXX") || fail 'could not stage installed binary'
cp "$source_bin" "$stage_bin"
chmod 755 "$stage_bin"
mv -f "$stage_bin" "$install_dir/$binary"
stage_bin=
printf '%s %s\n' "$install_action" "$install_dir/$binary"

case ":$PATH:" in
    *":$install_dir:"*) printf '%s is already on PATH.\n' "$install_dir" ;;
    *)
        # Single-quote the directory so shell metacharacters stay literal.
        quoted_dir=$(printf '%s' "$install_dir" | sed "s/'/'\\\\''/g")
        line="export PATH='$quoted_dir':\"\$PATH\" # taid installer"
        if [ -f "$rc" ] && grep -Fqx "$line" "$rc"; then
            printf 'PATH is already configured in %s. Open a new shell or run:\n  . %s\n' "$rc" "$rc"
        else
            printf '\n%s\n' "$line" >> "$rc"
            printf 'Added taid to PATH in %s. Open a new shell or run:\n  . %s\n' "$rc" "$rc"
        fi
        ;;
esac
