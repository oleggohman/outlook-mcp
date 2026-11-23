#!/bin/bash
# Launcher for the hxprobe MCP server when this repository is installed as a
# Claude Code plugin. It finds or installs the `hxprobe` program for the
# version in hxprobe/Cargo.toml and replaces itself with `hxprobe mcp`.
#
# stdout is the JSON-RPC channel of the server, so this script writes nothing
# to it: stdout is parked on fd 3 and only handed back to the server at exec.
#
# Environment:
#   HXPROBE_BIN           run this program instead of installing one
#   CLAUDE_PLUGIN_DATA    data directory (set by Claude Code)
#   HXPROBE_RELEASE_BASE  base URL of the release downloads
#   HXPROBE_SUMS          checksum file (default: release/SHA256SUMS)
#
# Written for the macOS system bash (3.2): no arrays, no `"$@"` under set -u.

set -euo pipefail

# Keep the real stdout on fd 3; everything else in this script goes to stderr.
exec 3>&1 1>&2

log() { printf 'hxprobe plugin: %s\n' "$*" >&2; }
die() { log "$*"; exit 1; }

run_server() {
    # $1 = program; the remaining arguments of the script follow `mcp`.
    local prog=$1
    shift
    exec "$prog" mcp ${1+"$@"} 1>&3 3>&-
}

# a. Explicit program.
if [ -n "${HXPROBE_BIN:-}" ]; then
    if [ -f "$HXPROBE_BIN" ] && [ -x "$HXPROBE_BIN" ]; then
        run_server "$HXPROBE_BIN" ${1+"$@"}
    fi
    die "HXPROBE_BIN is set to '$HXPROBE_BIN', which is not an executable file. Fix the path or unset HXPROBE_BIN."
fi

# b. Version, data directory and program path.
script_dir=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
repo=$(dirname "$script_dir")
manifest="$repo/hxprobe/Cargo.toml"
[ -f "$manifest" ] || die "cannot find $manifest"

# The first `version = "..."` inside the [package] table.
version=$(awk '
    /^[[:space:]]*\[/ { in_pkg = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*$/); next }
    in_pkg && /^[[:space:]]*version[[:space:]]*=/ {
        if (match($0, /"[^"]*"/)) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
    }
' "$manifest")
case "$version" in
    '' ) die "no version found in the [package] table of $manifest" ;;
    *[!0-9A-Za-z.+-]* ) die "unexpected version '$version' in $manifest" ;;
esac

data=${CLAUDE_PLUGIN_DATA:-$HOME/Library/Application Support/hxprobe-plugin}
bindir="$data/bin"
bin="$bindir/hxprobe-$version"
(umask 077 && mkdir -p "$bindir") || die "cannot create $bindir"
chmod 700 "$data" "$bindir" 2>/dev/null || true

# c. Already installed.
if [ -f "$bin" ] && [ -x "$bin" ]; then
    run_server "$bin" ${1+"$@"}
fi

# Install lock: a directory, created atomically by mkdir. It holds the pid of
# the owner. A lock whose owner is gone, or that is older than the stale
# timeout, is broken.
lock="$data/install.lock"
lock_stale_mins=30
lock_held=0
tmp_file=''

cleanup() {
    if [ -n "$tmp_file" ]; then rm -f "$tmp_file"; fi
    if [ "$lock_held" = 1 ]; then rm -rf "$lock"; lock_held=0; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

lock_is_stale() {
    local pid out
    [ -d "$lock" ] || return 1
    if [ -n "$(find "$lock" -maxdepth 0 -mmin +"$lock_stale_mins" 2>/dev/null)" ]; then
        return 0
    fi
    pid=$(cat "$lock/pid" 2>/dev/null || true)
    case "$pid" in
        '' | *[!0-9]* ) return 1 ;;  # owner still writing its pid, or unknown
    esac
    # kill -0 also fails when the process exists but belongs to someone else;
    # only "No such process" means the owner is gone.
    if out=$(kill -0 "$pid" 2>&1); then return 1; fi
    case "$out" in
        *'No such process'* ) return 0 ;;
    esac
    return 1
}

acquire_lock() {
    local waited=0
    while ! mkdir "$lock" 2>/dev/null; do
        if lock_is_stale; then
            log "removing a stale install lock"
            # Rename first so that only one waiter removes it.
            mv "$lock" "$lock.stale.$$" 2>/dev/null && rm -rf "$lock.stale.$$"
            continue
        fi
        if [ "$waited" = 0 ]; then
            log "another start is installing hxprobe $version; waiting"
        fi
        waited=$((waited + 1))
        sleep 1
    done
    lock_held=1
    printf '%s\n' "$$" > "$lock/pid"
}

release_lock() {
    if [ "$lock_held" = 1 ]; then rm -rf "$lock"; lock_held=0; fi
}

# Move a verified or freshly built file into place, drop other versions and
# start the server.
install_and_run() {
    local src=$1 f
    shift
    chmod 700 "$src"
    mv -f "$src" "$bin"
    tmp_file=''
    for f in "$bindir"/hxprobe-*; do
        [ -e "$f" ] || continue
        [ "$f" = "$bin" ] && continue
        rm -f "$f"
    done
    release_lock
    log "installed hxprobe $version at $bin"
    run_server "$bin" ${1+"$@"}
}

acquire_lock
# Another start may have finished the install while this one waited.
if [ -f "$bin" ] && [ -x "$bin" ]; then
    release_lock
    run_server "$bin" ${1+"$@"}
fi

# d. Verified prebuilt program from the release.
try_download() {
    local arch asset sums want base url proto got
    [ "$(uname -s)" = Darwin ] || return 1
    case "$(uname -m)" in
        arm64 | aarch64 ) arch=arm64 ;;
        x86_64 ) arch=x86_64 ;;
        * ) return 1 ;;
    esac
    asset="hxprobe-$version-macos-$arch"
    sums=${HXPROBE_SUMS:-$repo/release/SHA256SUMS}
    [ -f "$sums" ] || return 1
    # Lines are `<sha256>  <asset>` (an optional `*` before the name is the
    # binary-mode marker of shasum). Blank lines and `#` comments are skipped.
    want=$(awk -v a="$asset" '
        /^[[:space:]]*#/ || NF == 0 { next }
        NF == 2 { n = $2; sub(/^\*/, "", n); if (n == a) { print tolower($1); exit } }
    ' "$sums")
    [ -n "$want" ] || return 1
    case "$want" in
        *[!0-9a-f]* ) log "bad checksum for $asset in $sums"; return 1 ;;
    esac
    [ "${#want}" = 64 ] || { log "bad checksum for $asset in $sums"; return 1; }
    command -v curl >/dev/null 2>&1 || { log "curl not found; cannot download $asset"; return 1; }
    command -v shasum >/dev/null 2>&1 || { log "shasum not found; cannot verify $asset"; return 1; }

    base=${HXPROBE_RELEASE_BASE:-https://github.com/oleggohman/outlook-mcp/releases/download}
    base=${base%/}
    case "$base" in
        file://* ) proto='=file' ;;
        * ) proto='=https' ;;
    esac
    url="$base/v$version/$asset"
    tmp_file=$(mktemp "$bindir/.download.XXXXXX") || return 1
    log "downloading $url"
    if ! curl -fsSL --proto "$proto" --proto-redir "$proto" --tlsv1.2 -o "$tmp_file" "$url"; then
        log "download of $asset failed"
        rm -f "$tmp_file"; tmp_file=''
        return 1
    fi
    got=$(shasum -a 256 "$tmp_file" | awk '{ print tolower($1) }')
    if [ "$got" != "$want" ]; then
        log "checksum mismatch for $asset (expected $want, got $got); the file was deleted and not run"
        rm -f "$tmp_file"; tmp_file=''
        return 1
    fi
    log "checksum of $asset verified"
    install_and_run "$tmp_file" ${1+"$@"}
}

try_download ${1+"$@"} || true

# e. Build from source with cargo.
if command -v cargo >/dev/null 2>&1; then
    log "building hxprobe $version with cargo (first start only, about a minute)"
    target="$data/target"
    if ! cargo build --release --locked \
            --manifest-path "$manifest" --target-dir "$target" >&2; then
        die "cargo build failed; see the messages above"
    fi
    built="$target/release/hxprobe"
    [ -f "$built" ] && [ -x "$built" ] || die "cargo build did not produce $built"
    tmp_file=$(mktemp "$bindir/.build.XXXXXX")
    cp "$built" "$tmp_file"
    # The build tree is only needed for the build; it takes hundreds of MB.
    rm -rf "$target"
    install_and_run "$tmp_file" ${1+"$@"}
fi

# f. Nothing worked.
die "no prebuilt hxprobe $version is available for this machine and cargo is not on PATH. Install Rust from https://rustup.rs and restart the MCP server, or set HXPROBE_BIN to the path of an hxprobe program."
