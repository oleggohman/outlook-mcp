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
    exec "$prog" mcp ${1+"$@"} 1>&3 3>&- 9>&-
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

# Signals end the launcher at once: long commands (download, build, the wait
# for the lock) run in the background and the launcher waits for them, so a
# trap can stop the command and clean up.
child=''
starting=0
pending=''
tmp_file=''
build_dir=''

# Runs at exit; a failing rm must not replace the exit status.
cleanup() {
    if [ -n "$tmp_file" ]; then rm -f "$tmp_file" 2>/dev/null || true; fi
    if [ -n "$build_dir" ]; then rm -rf "$build_dir" 2>/dev/null || true; fi
}
on_signal() {
    local i=0
    # A command is being started and `child` is not set yet: whoever starts
    # it calls on_signal again as soon as it is.
    if [ "$starting" = 1 ]; then pending=$1; return 0; fi
    trap '' INT TERM HUP
    if [ -n "$child" ]; then
        # The whole process group of the command (cargo does not pass a
        # signal on to the compilers it started), or the single process when
        # it has no group of its own.
        kill -TERM -- -"$child" 2>/dev/null || kill "$child" 2>/dev/null || true
        # Give the group 2 s to end, then kill what is left, so that nothing
        # writes into the build directory while cleanup removes it.
        while kill -0 -- -"$child" 2>/dev/null && [ "$i" -lt 20 ]; do
            sleep 0.1
            i=$((i + 1))
        done
        kill -KILL -- -"$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
        i=0
        while kill -0 -- -"$child" 2>/dev/null && [ "$i" -lt 10 ]; do
            sleep 0.1
            i=$((i + 1))
        done
    fi
    exit "$1"
}
trap cleanup EXIT
trap 'on_signal 130' INT
trap 'on_signal 143' TERM
trap 'on_signal 129' HUP

# Runs a command without the lock (fd 9) and the real stdout (fd 3), and
# returns its exit status. The command gets a process group of its own
# (`set -m`), so a signal can stop it together with everything it started.
# With job control a background command would keep the stdin of the launcher,
# which belongs to the server, so it reads /dev/null instead.
run_child() {
    local rc=0
    starting=1
    set -m
    "$@" </dev/null 3>&- 9>&- &
    child=$!
    set +m
    starting=0
    if [ -n "$pending" ]; then on_signal "$pending"; fi
    wait "$child" || rc=$?
    child=''
    return "$rc"
}

# Install lock: an flock(2) lock on fd 9, taken with lockf (macOS) or flock
# (Linux). The kernel drops it when the launcher ends, however it ends, so
# there is no stale lock to detect. Without either tool the install runs
# unlocked; build directories are per process, so two installs at once cost
# time but do not disturb each other.
lock="$data/install.lock"
lock_held=0

acquire_lock() {
    local try block rc=0
    if command -v lockf >/dev/null 2>&1; then
        try='lockf -s -t 0 9'; block='lockf -s 9'
    elif command -v flock >/dev/null 2>&1; then
        try='flock -n -E 75 9'; block='flock 9'
    else
        log "neither lockf nor flock found; installing without a lock"
        return 0
    fi
    # An earlier launcher used a directory of this name holding a `pid` file.
    if [ -d "$lock" ]; then
        rm -f "$lock/pid" 2>/dev/null || true
        rmdir "$lock" 2>/dev/null || true
    fi
    exec 9>>"$lock" || die "cannot open $lock"
    $try 3>&- || rc=$?
    if [ "$rc" = 75 ]; then
        log "another start is installing hxprobe $version; waiting"
        rc=0
        starting=1
        $block 3>&- &
        child=$!
        starting=0
        if [ -n "$pending" ]; then on_signal "$pending"; fi
        wait "$child" || rc=$?
        child=''
    fi
    if [ "$rc" != 0 ]; then
        # For example a lockf that cannot lock a file descriptor.
        exec 9>&-
        log "cannot lock $lock (status $rc); installing without a lock"
        return 0
    fi
    lock_held=1
}

release_lock() {
    if [ "$lock_held" = 1 ]; then exec 9>&-; lock_held=0; fi
}

# Succeeds when the process with pid $1 no longer exists.
pid_gone() {
    local out
    case "$1" in
        '' | *[!0-9]* ) return 1 ;;
    esac
    # kill -0 also fails when the process exists but belongs to someone else;
    # only "No such process" means it is gone.
    if out=$(kill -0 "$1" 2>&1); then return 1; fi
    case "$out" in
        *'No such process'* ) return 0 ;;
    esac
    return 1
}

# Remove build directories that earlier starts left behind. With the lock
# held nobody else is building, so every one of them is a leftover; without
# it only those of processes that are gone.
remove_old_build_dirs() {
    local d
    if [ "$lock_held" = 1 ]; then rm -rf "$data/target"; fi
    for d in "$data"/target.*; do
        [ -d "$d" ] || continue
        if [ "$lock_held" = 1 ] || pid_gone "${d##*.}"; then rm -rf "$d"; fi
    done
}

# Move a verified or freshly built file into place, drop other versions and
# start the server.
install_and_run() {
    local src=$1 f
    shift
    chmod 700 "$src" || die "cannot make $src executable"
    mv -f "$src" "$bin" || die "cannot move $src to $bin"
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
remove_old_build_dirs

# d. Verified prebuilt program from the release. On success the file is at
# $tmp_file.
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
    if ! run_child curl -fsSL --proto "$proto" --proto-redir "$proto" --tlsv1.2 \
            --connect-timeout 15 --max-time 300 -o "$tmp_file" "$url"; then
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
}

# The install itself runs outside the `if` condition, where set -e applies.
if try_download; then
    install_and_run "$tmp_file" ${1+"$@"}
fi

# e. Build from source with cargo, in a directory of this process.
if command -v cargo >/dev/null 2>&1; then
    log "building hxprobe $version with cargo (first start only, about a minute)"
    build_dir="$data/target.$$"
    if ! run_child cargo build --release --locked \
            --manifest-path "$manifest" --target-dir "$build_dir"; then
        die "cargo build failed; see the messages above"
    fi
    built="$build_dir/release/hxprobe"
    [ -f "$built" ] && [ -x "$built" ] || die "cargo build did not produce $built"
    tmp_file=$(mktemp "$bindir/.build.XXXXXX")
    cp "$built" "$tmp_file"
    # The build tree is only needed for the build; it takes hundreds of MB.
    rm -rf "$build_dir"
    build_dir=''
    install_and_run "$tmp_file" ${1+"$@"}
fi

# f. Nothing worked.
die "no prebuilt hxprobe $version is available for this machine and cargo is not on PATH. Install Rust from https://rustup.rs and restart the MCP server, or set HXPROBE_BIN to the path of an hxprobe program."
