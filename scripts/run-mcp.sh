#!/bin/bash
# Launcher for the hxprobe MCP server when this repository is installed as a
# Claude Code plugin. It finds or installs the `hxprobe` program for the
# version in hxprobe/Cargo.toml and replaces itself with `hxprobe mcp`.
#
# The program lives at one stable path, <install dir>/bin/hxprobe, for every
# version, so one Full Disk Access entry serves every client and upgrade. The
# installed version is asked with `hxprobe --version`: the wanted version or
# a newer one is run as it is (never downgrade); a missing or older program,
# or one without an answer (0.1.0), is replaced by a rename under the install
# lock. The server starts its refresh service from its own path, so it is
# always started from the stable path.
#
# stdout is the JSON-RPC channel of the server, so this script writes nothing
# to it: stdout is parked on fd 3 and only handed back to the server at exec.
#
# Environment:
#   HXPROBE_BIN           run this program instead of installing one
#   HXPROBE_INSTALL_DIR   install directory (default:
#                         $HOME/Library/Application Support/hxprobe; required
#                         when HOME is not set)
#   CLAUDE_PLUGIN_DATA    plugin data directory (set by Claude Code); only
#                         what earlier launchers left there is removed after
#                         an install: bin/hxprobe-<version>, target,
#                         target.<pid> and install.lock
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

# b. Version, install directory and program path.
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

# HOME is only needed for the defaults; it may be unset or empty.
home=${HOME:-}
inst=${HXPROBE_INSTALL_DIR:-}
if [ -z "$inst" ]; then
    [ -n "$home" ] || die "HOME is not set; set HXPROBE_INSTALL_DIR"
    inst="$home/Library/Application Support/hxprobe"
fi
bindir="$inst/bin"
bin="$bindir/hxprobe"
# Earlier launchers installed <plugin data>/bin/hxprobe-<version>. Empty when
# the plugin data directory is not known.
legacy=${CLAUDE_PLUGIN_DATA:-}
if [ -z "$legacy" ] && [ -n "$home" ]; then
    legacy="$home/Library/Application Support/hxprobe-plugin"
fi
if [ -n "$legacy" ]; then legacy="$legacy/bin"; fi
(umask 077 && mkdir -p "$bindir") || die "cannot create $bindir"
# The program run from here reads the user's mail: the directories must be
# the user's own and closed to everyone else.
if ! [ -O "$inst" ] || ! [ -O "$bindir" ]; then
    die "$inst or $bindir is not owned by the current user; set HXPROBE_INSTALL_DIR to a directory of your own"
fi
chmod 700 "$inst" "$bindir" || die "cannot set the mode of $inst and $bindir"

# Prints -1, 0 or 1 when version $1 is older than, equal to or newer than $2,
# comparing major, minor and patch as numbers. With equal numbers a
# pre-release (a suffix after `-`) is older than the version without one, and
# two pre-releases compare as text; build metadata after `+` is ignored.
# Prints `?` when either is not of that form.
version_cmp() {
    awk -v a="$1" -v b="$2" '
        function parse(v, p,    n) {
            sub(/[-+].*$/, "", v)
            if (v !~ /^[0-9]+\.[0-9]+\.[0-9]+$/) return 0
            n = split(v, p, ".")
            return n == 3
        }
        function pre(v,    i) {
            sub(/\+.*$/, "", v)
            i = index(v, "-")
            return i ? substr(v, i + 1) : ""
        }
        BEGIN {
            if (!parse(a, x) || !parse(b, y)) { print "?"; exit }
            for (i = 1; i <= 3; i++) {
                if (x[i] + 0 < y[i] + 0) { print -1; exit }
                if (x[i] + 0 > y[i] + 0) { print 1; exit }
            }
            pa = pre(a); pb = pre(b)
            if (pa == pb) print 0
            else if (pa == "") print 1
            else if (pb == "") print -1
            else print ((pa "") < (pb "")) ? -1 : 1
        }'
}

# The version that the program at $bin reports (`hxprobe <version>` from
# `--version`), or nothing when it is missing or gives no such answer.
installed_version() {
    local out
    [ -f "$bin" ] && [ -x "$bin" ] || return 0
    out=$("$bin" --version </dev/null 2>/dev/null 3>&- 9>&-) || return 0
    case "$out" in
        'hxprobe '* ) out=${out#hxprobe } ;;
        * ) return 0 ;;
    esac
    case "$out" in
        '' | *[!0-9A-Za-z.+-]* ) return 0 ;;
    esac
    printf '%s\n' "$out"
}

# Succeeds when the installed program is to be run: it reports the wanted
# version or a newer one. A newer one is kept (never downgrade: clients with
# different plugin versions would otherwise reinstall on every start).
installed_ok() {
    local have
    have=$(installed_version)
    [ -n "$have" ] || return 1
    case "$(version_cmp "$have" "$version")" in
        0 ) return 0 ;;
        1 ) log "hxprobe $have at $bin is newer than $version of this plugin; running it"
            return 0 ;;
    esac
    return 1
}

# c. Already installed.
if installed_ok; then
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
lock="$inst/install.lock"
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
# it only those of processes that are gone. The same for the temp files of
# a launcher that was killed, which carry no pid: only with the lock held.
remove_old_build_dirs() {
    local d
    if [ "$lock_held" = 1 ]; then
        rm -f "$bindir"/.download.* "$bindir"/.build.* 2>/dev/null || true
    fi
    for d in "$inst"/target.*; do
        [ -d "$d" ] || continue
        if [ "$lock_held" = 1 ] || pid_gone "${d##*.}"; then rm -rf "$d"; fi
    done
}

# Move a verified or freshly built file into place, remove the programs of
# earlier launchers and start the server. The move is a rename in one
# directory: atomic, and a new file, so running copies of the old program
# keep their file (overwriting a signed program in place would invalidate
# the signature of running copies).
install_and_run() {
    local src=$1 f d
    shift
    chmod 700 "$src" || die "cannot make $src executable"
    # mv would put the file into a directory (or a link to one) of that name.
    [ ! -d "$bin" ] || die "$bin is a directory; remove it and start again"
    mv -f "$src" "$bin" || die "cannot move $src to $bin"
    tmp_file=''
    if [ -n "$legacy" ]; then
        for f in "$legacy"/hxprobe-*; do
            [ -f "$f" ] || continue
            rm -f "$f" || true
        done
        # They also built and locked in the plugin data directory. Not when
        # that is the install directory: its lock is the one held here.
        d=${legacy%/bin}
        if [ "$d" != "$inst" ]; then
            for f in "$d/target" "$d"/target.*; do
                [ -d "$f" ] || continue
                rm -rf "$f" 2>/dev/null || true
            done
            rm -f "$d/install.lock/pid" "$d/install.lock" 2>/dev/null || true
            rmdir "$d/install.lock" 2>/dev/null || true
        fi
    fi
    release_lock
    log "installed hxprobe $version at $bin"
    run_server "$bin" ${1+"$@"}
}

acquire_lock
# Another start may have finished the install while this one waited.
if installed_ok; then
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
    log "note: a program built here is not signed with the release identity, so a Full Disk Access grant for $bin may not survive the next build"
    build_dir="$inst/target.$$"
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
