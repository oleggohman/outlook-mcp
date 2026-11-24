#!/bin/bash
# Launcher for the hxprobe MCP server when this repository is installed as a
# Claude Code plugin. It finds or installs the `hxprobe` program for the
# version in hxprobe/Cargo.toml and replaces itself with `hxprobe mcp`.
#
# The program lives at one stable path, <install dir>/bin/hxprobe, for every
# version, so one Full Disk Access entry serves every client and upgrade. At
# every start (unless HXPROBE_BIN is set) the SHA-256 of the installed
# program is looked up in the checksum file among the assets of this
# architecture: a listed program has the version of its asset name and is
# not run to ask it. Only a program that is not listed (one built here, one
# of a newer plugin, or any program when shasum is missing) is asked with
# `hxprobe --version`. Either way the wanted version or a newer one is run
# as it is (never downgrade); a missing or older program, or one without an
# answer (0.1.0), is replaced by a rename under the install lock. A release
# program whose sum is no longer the one recorded at its install (see
# bin/.origin below), whatever version that was, changed after the install:
# it is never run, not even to ask it, the release program is downloaded
# again, and the launcher stops when that fails (without a release program
# for this version it is replaced by a build). In a directory of the
# user's own this finds a damaged or half-written file; it is not a security
# boundary, since whoever can write the directory can write bin/.origin as
# well: that boundary is the owner and mode of the directory. The server
# starts its refresh service from its own path, so it is always started from
# the stable path.
#
# The default install directory is /Users/Shared/hxprobe, the same path for
# every account, so that one device profile can grant Full Disk Access to
# /Users/Shared/hxprobe/bin/hxprobe. The server starts its service from that
# file on every service start, for as long as it runs, so only a directory
# that nobody else can change is used. Its parent must be owned by root or
# the current user and be sticky or closed to group and others. The chain
# directory, bin, program is then read with `ls -ldn` and classified:
#   absent  the directory does not exist: it is made (umask 022) and the
#           chain is classified again from what is on disk;
#   own     every element a real directory or regular file of the current
#           user, not writable by group or others, without an access list:
#           used with all of the rules below; its directories and the
#           program get mode 0755, bin/.origin 0644, install.lock 0600;
#   root    the same, owned by root, while the launcher runs as another
#           user (run as root, such a chain is its own: `own`, and written
#           to): only read. Its program is run when its
#           SHA-256 is listed with the wanted version, or with an older one
#           (with a log line: an administrator has to update it); nothing
#           is written there, no lock is taken, nothing is downloaded. A
#           missing program or one that is not listed (also a newer one,
#           and any program when shasum is missing) is not run;
#   other   anything else (a link anywhere, another file type, a write bit
#           for group or others, an access list, another account, mixed
#           owners): not used. An own directory that is or was open to
#           others is not tightened, since its content can no longer be
#           vouched for.
# When the shared directory is not used, the launcher says why on stderr and
# falls back to $HOME/Library/Application Support/hxprobe, the user's own
# directory with mode 0700. An explicit HXPROBE_INSTALL_DIR follows the rules
# of that fallback directory. After a fallback the same sentence is passed to
# the server in HXPROBE_INSTALL_NOTE, which appends it to its permission
# errors (up to 300 characters of it, so no reason names a path inside the
# plugin); in every other case that variable is removed from the server's
# environment. The first install into the shared directory says once that
# a program left by an earlier version under $HOME is no longer used; it is
# not removed.
#
# Where the installed program came from is recorded next to it in
# bin/.origin: `release <sha256>` for a verified download, `build` for a
# cargo build (no record counts as `build`). A built program of the wanted
# version is replaced by the release program as soon as the checksum file
# lists one. A release file that does not match its checksum is deleted and
# never run, and nothing is built in its place: the launcher stops, unless a
# built program of the wanted version is installed, which is then run. A
# download that fails is followed by a build only when no program that
# answers is installed; otherwise the installed one is run. When the release
# program could not be had and a built program of the wanted version is run
# instead, bin/.download-failed is left and the download is not tried again
# while that file is younger than about 6 hours.
#
# stdout is the JSON-RPC channel of the server, so this script writes nothing
# to it: stdout is parked on fd 3 and only handed back to the server at exec.
#
# Environment:
#   HXPROBE_BIN           run this program instead of installing one
#   HXPROBE_INSTALL_DIR   install directory (default: the shared directory,
#                         or $HOME/Library/Application Support/hxprobe when
#                         that is not used; required when neither can be
#                         used)
#   HXPROBE_SHARED_DIR    replaces the shared directory /Users/Shared/hxprobe;
#                         meant for tests
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
    # The server appends the note to its permission errors; an inherited
    # value is never passed on.
    if [ -n "${install_note:-}" ]; then
        export HXPROBE_INSTALL_NOTE="$install_note"
    else
        unset HXPROBE_INSTALL_NOTE
    fi
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

# The architecture of the release programs for this machine (arm64 or
# x86_64); fails on another system or machine.
release_arch() {
    [ "$(uname -s)" = Darwin ] || return 1
    case "$(uname -m)" in
        arm64 | aarch64 ) echo arm64 ;;
        x86_64 ) echo x86_64 ;;
        * ) return 1 ;;
    esac
}

# The checksum file: lines `<sha256>  <asset>`.
sums_file=${HXPROBE_SUMS:-$repo/release/SHA256SUMS}

# The SHA-256 of file $1 in lower case, or nothing (no shasum, unreadable).
file_sum() {
    command -v shasum >/dev/null 2>&1 || return 0
    shasum -a 256 "$1" 2>/dev/null | awk 'NR == 1 && length($1) == 64 && $1 !~ /[^0-9a-fA-F]/ { print tolower($1) }' || true
}

# The version that the checksum file lists for program $1, from the name of
# the asset of this architecture whose sum is the program's
# (hxprobe-<version>-macos-<arch>), or nothing when it is not listed. The
# program is not run. Without shasum nothing is listed.
listed_version() {
    local arch sum v
    [ -f "$1" ] && [ -f "$sums_file" ] || return 0
    arch=$(release_arch) || return 0
    sum=$(file_sum "$1")
    [ -n "$sum" ] || return 0
    v=$(awk -v s="$sum" -v pre="hxprobe-" -v post="-macos-$arch" '
        /^[[:space:]]*#/ || NF == 0 { next }
        NF == 2 && tolower($1) == s {
            n = $2; sub(/^\*/, "", n)
            if (substr(n, 1, length(pre)) == pre && length(n) > length(pre) + length(post) \
                && substr(n, length(n) - length(post) + 1) == post) {
                print substr(n, length(pre) + 1, length(n) - length(pre) - length(post)); exit
            }
        }' "$sums_file")
    case "$v" in
        '' | *[!0-9A-Za-z.+-]* ) return 0 ;;
    esac
    printf '%s\n' "$v"
}

# The default install directory, shared by every account of the Mac, so that
# one device profile can grant Full Disk Access to its program.
shared_default=/Users/Shared/hxprobe
# The effective uid, which `-O` compares with too; from bash, not `id`.
myuid=$EUID

# Reads the owner uid, the mode string and the access-list mark of path $1
# (not following a link) from `ls -ldn`: sets pi_type (`d`, `-`, `l`, ...),
# pi_mode (the 10 characters `drwxr-xr-x`), pi_acl (yes or no) and pi_uid.
# Fails when nothing exists at $1. On macOS `-e` also lists the entries of an
# access list on lines of their own: the `+` mark is not shown for a file
# that has extended attributes as well (`@` takes its place).
path_info() {
    local out first m uid
    out=$(ls -ldne "$1" 2>/dev/null) || out=$(ls -ldn "$1" 2>/dev/null) || return 1
    first=${out%%$'\n'*}
    read -r m _ uid _ <<< "$first"
    pi_type=${m:0:1}
    pi_mode=${m:0:10}
    pi_uid=$uid
    pi_acl=no
    if [ "${m:10:1}" = + ] || [ "$first" != "$out" ]; then pi_acl=yes; fi
    case "$pi_uid" in
        '' | *[!0-9]* ) return 1 ;;
    esac
}

# Succeeds when the mode in pi_mode has a write bit for group or others.
pi_open() { [ "${pi_mode:5:1}" != - ] || [ "${pi_mode:8:1}" != - ]; }

# Succeeds when directory $1, the parent of the shared directory, is safe: a
# real directory owned by root or the current user, and either sticky (as
# /Users/Shared, mode 1777) or not writable by group and others. Otherwise
# `reason` says why.
parent_safe() {
    if ! path_info "$1"; then reason="$1 does not exist"; return 1; fi
    if [ "$pi_type" != d ]; then reason="$1 is not a directory"; return 1; fi
    case "$pi_uid" in
        0 | "$myuid" ) ;;
        * ) reason="$1 is owned by another account (uid $pi_uid)"; return 1 ;;
    esac
    case "${pi_mode:9:1}" in
        t | T ) return 0 ;;
    esac
    if pi_open; then reason="$1 is writable by others and not sticky"; return 1; fi
}

# Classifies the chain <dir>, <dir>/bin, <dir>/bin/hxprobe for shared
# directory $1 from what is on disk. Sets `chain` to
#   absent  <dir> does not exist;
#   own     every element that exists is a real directory (the program a
#           regular file), owned by the current user, not writable by group
#           or others and without an access list; `bin` and the program may
#           be missing;
#   root    the same, owned by root, when the current user is not root
#           (for root such a chain is `own`);
#   bad     anything else (a link anywhere, another file type, a write bit
#           for group or others, an access list, another account, mixed
#           owners); `reason` says why.
classify_shared() {
    local p kind owner=''
    for p in "$1" "$1/bin" "$1/bin/hxprobe"; do
        kind=d
        if [ "$p" = "$1/bin/hxprobe" ]; then kind=-; fi
        if ! path_info "$p"; then
            if [ "$p" = "$1" ]; then chain=absent; return 0; fi
            break
        fi
        chain=bad
        if [ "$pi_type" = l ]; then reason="$p is a symbolic link"; return 0; fi
        if [ "$pi_type" != "$kind" ]; then
            if [ "$kind" = d ]; then reason="$p is not a directory"; else reason="$p is not a regular file"; fi
            return 0
        fi
        if pi_open; then reason="$p is writable by group or others"; return 0; fi
        if [ "$pi_acl" = yes ]; then reason="$p has an access control list"; return 0; fi
        case "$pi_uid" in
            0 | "$myuid" ) ;;
            * ) reason="$p is owned by another account (uid $pi_uid)"; return 0 ;;
        esac
        if [ -z "$owner" ]; then
            owner=$pi_uid
        elif [ "$pi_uid" != "$owner" ]; then
            reason="$p and $1 have different owners"
            return 0
        fi
    done
    if [ "$owner" = "$myuid" ]; then chain=own; else chain=root; fi
}

# Succeeds when shared directory $1 is to be used: its parent is safe and the
# chain is the user's own, or root's with a program whose SHA-256 is listed
# (`chain` tells which). An absent directory (and a missing `bin` in it) is
# made with mkdir under umask 022, and the chain is then classified again
# from what is on disk, whether mkdir succeeded or not: a directory or link
# that another account put there in between is judged like any other. A
# root chain is only read: nothing is made or written there. Otherwise
# `reason` says why it is not used.
shared_usable() {
    case "$1" in
        /?* ) ;;
        * ) reason="$1 is not an absolute path"; return 1 ;;
    esac
    parent_safe "$(dirname "$1")" || return 1
    classify_shared "$1"
    if [ "$chain" = absent ]; then
        (umask 022 && mkdir "$1") 2>/dev/null || true
        classify_shared "$1"
        if [ "$chain" = absent ]; then reason="cannot create $1"; return 1; fi
    fi
    if [ "$chain" = own ] && ! [ -e "$1/bin" ] && ! [ -L "$1/bin" ]; then
        (umask 022 && mkdir "$1/bin") 2>/dev/null || true
        classify_shared "$1"
    fi
    case "$chain" in
        own ) return 0 ;;
        root ) root_listed "$1/bin/hxprobe" ;;
        * ) return 1 ;;
    esac
}

# Succeeds when program $1 of a root-owned chain is to be run: its SHA-256
# is listed with the wanted version, or with an older one (logged: only an
# administrator can update it). A program that is not listed, also a newer
# one this plugin does not know, is not run; `reason` says why.
root_listed() {
    local have
    if ! [ -e "$1" ]; then reason="$(dirname "$(dirname "$1")") is owned by root and $1 does not exist"; return 1; fi
    if ! [ -x "$1" ]; then reason="$1 is owned by root and is not executable"; return 1; fi
    if ! command -v shasum >/dev/null 2>&1; then
        reason="$1 is owned by root and shasum is not available to check its SHA-256"
        return 1
    fi
    have=$(listed_version "$1")
    if [ -z "$have" ]; then
        # Not the full path of the checksum file: the reason goes into the
        # note for the server, which cuts it after 300 characters.
        reason="$1 is owned by root and its SHA-256 is not listed for this machine in release/SHA256SUMS"
        return 1
    fi
    case "$(version_cmp "$have" "$version")" in
        0 ) ;;
        -1 ) log "hxprobe $have at $1 is older than $version of this plugin; running it. It is owned by root: an administrator has to update it" ;;
        1 ) log "hxprobe $have at $1 is newer than $version of this plugin; running it" ;;
        * ) reason="$1 is owned by root and its listed version '$have' cannot be compared"; return 1 ;;
    esac
}

# HOME is only needed for the per-user defaults; it may be unset or empty.
home=${HOME:-}
inst=${HXPROBE_INSTALL_DIR:-}
# Why the shared directory was not used; empty when it was, or when an
# install directory was given.
install_note=''
# Modes in the install directory: the shared directory is readable by every
# account (one program for all of them, see the header); every other one is
# the user's own and closed to everyone else. origin_mode is empty when the
# mode of bin/.origin is left as it is.
dir_mode=700
prog_mode=700
origin_mode=''
mkdir_umask=077
in_shared=0
if [ -z "$inst" ]; then
    shared=${HXPROBE_SHARED_DIR:-$shared_default}
    reason=''
    if shared_usable "$shared"; then
        # A verified root-owned program is run as it is, before anything
        # could be written to its directory.
        if [ "$chain" = root ]; then
            run_server "$shared/bin/hxprobe" ${1+"$@"}
        fi
        inst=$shared
        in_shared=1
        dir_mode=755
        prog_mode=755
        origin_mode=644
        mkdir_umask=022
    else
        # Fall back to the user's own directory, with today's rules.
        install_note="the shared program $shared/bin/hxprobe was not used ($reason); a device profile for that path does not cover this program"
        [ -n "$home" ] || die "$install_note. HOME is not set; set HXPROBE_INSTALL_DIR"
        inst="$home/Library/Application Support/hxprobe"
        log "$install_note; using $inst/bin/hxprobe"
    fi
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
(umask "$mkdir_umask" && mkdir -p "$bindir") || die "cannot create $bindir"
# The program run from here reads the user's mail: the directories must be
# the user's own, and closed to everyone else or (the shared directory) not
# writable by anyone else.
if ! [ -O "$inst" ] || ! [ -O "$bindir" ]; then
    die "$inst or $bindir is not owned by the current user; set HXPROBE_INSTALL_DIR to a directory of your own"
fi
chmod "$dir_mode" "$inst" "$bindir" || die "cannot set the mode of $inst and $bindir"

# The version that the program $1 reports (`hxprobe <version>` from
# `--version`), or nothing when it is missing or gives no such answer.
program_version() {
    local out
    [ -f "$1" ] && [ -x "$1" ] || return 0
    out=$("$1" --version </dev/null 2>/dev/null 3>&- 9>&-) || return 0
    case "$out" in
        'hxprobe '* ) out=${out#hxprobe } ;;
        * ) return 0 ;;
    esac
    case "$out" in
        '' | *[!0-9A-Za-z.+-]* ) return 0 ;;
    esac
    printf '%s\n' "$out"
}

installed_version() { program_version "$bin"; }

# Where the program at $bin came from: `release` or `build` (see the header).
origin_file="$bindir/.origin"
installed_origin() {
    local o=''
    if [ -f "$origin_file" ]; then read -r o _ < "$origin_file" || true; fi
    if [ "$o" = release ]; then echo release; else echo build; fi
}

# Left when the release program could not be had while a built program of the
# wanted version is installed. While it is younger than 6 hours that program
# is run as it is: otherwise every start would wait for the download (up to
# the connect timeout) under the install lock, one start after the other.
backoff_file="$bindir/.download-failed"
backoff_active() {
    [ -f "$backoff_file" ] || return 1
    [ -n "$(find "$backoff_file" -mmin -360 2>/dev/null)" ]
}

# Looks up the release program for this machine in the checksum file, once.
# Succeeds when there is one; `asset` is then its name and `want` its SHA-256.
asset=''
want=''
sum_known=''
release_sum() {
    local arch sums=$sums_file
    if [ -n "$sum_known" ]; then [ "$sum_known" = yes ]; return; fi
    sum_known=no
    arch=$(release_arch) || return 1
    asset="hxprobe-$version-macos-$arch"
    [ -f "$sums" ] || return 1
    # Lines are `<sha256>  <asset>` (shasum may put a `*` before the name;
    # it is not part of it). Blank lines and `#` comments are skipped.
    want=$(awk -v a="$asset" '
        /^[[:space:]]*#/ || NF == 0 { next }
        NF == 2 { n = $2; sub(/^\*/, "", n); if (n == a) { print tolower($1); exit } }
    ' "$sums")
    [ -n "$want" ] || return 1
    case "$want" in
        *[!0-9a-f]* ) log "bad checksum for $asset in $sums"; return 1 ;;
    esac
    [ "${#want}" = 64 ] || { log "bad checksum for $asset in $sums"; return 1; }
    sum_known=yes
}

# The sum in the origin record (`release <sha256>`), or nothing.
origin_sum() {
    local o='' s=''
    if [ -f "$origin_file" ]; then read -r o s _ < "$origin_file" || true; fi
    if [ "$o" = release ]; then printf '%s\n' "$s"; fi
}

# Succeeds when the installed program is to be run. Its SHA-256 is looked up
# first: a listed program has the version of its asset name and is not run to
# ask it. Only a program that is not listed is asked with --version: it is to
# be run when it reports the wanted version or a newer one. A newer one is
# kept (never downgrade: clients with different plugin versions would
# otherwise reinstall on every start). A program of the wanted version that
# was built here is not run as it is while a release program is listed for
# it: `upgrade` is then 1, and the release program is tried first, unless
# that failed a short while ago. A release program whose sum is no longer
# the one in its origin record changed after the install, whatever version
# the record was written for: it is not run at all, `changed` is then 1.
upgrade=0
changed=0
installed_ok() {
    local have got was
    upgrade=0
    changed=0
    [ -f "$bin" ] && [ -x "$bin" ] || return 1
    have=$(listed_version "$bin")
    if [ -n "$have" ]; then
        case "$(version_cmp "$have" "$version")" in
            0 ) return 0 ;;
            1 ) log "hxprobe $have at $bin is newer than $version of this plugin; running it"
                return 0 ;;
        esac
        return 1
    fi
    got=$(file_sum "$bin")
    was=$(origin_sum)
    if [ -n "$got" ] && [ -n "$was" ] && [ "$got" != "$was" ]; then
        changed=1
        return 1
    fi
    have=$(installed_version)
    [ -n "$have" ] || return 1
    case "$(version_cmp "$have" "$version")" in
        0 ) if [ "$(installed_origin)" = build ] && release_sum && ! backoff_active; then
                upgrade=1
                return 1
            fi
            return 0 ;;
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
# time but do not disturb each other. The lock file has mode 0600 in every
# install directory.
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
    # chmod and the redirection below would follow a link to another file.
    [ ! -L "$lock" ] || die "$lock is a symbolic link; remove it and start again"
    # An earlier launcher used a directory of this name holding a `pid` file.
    if [ -d "$lock" ]; then
        rm -f "$lock/pid" 2>/dev/null || true
        rmdir "$lock" 2>/dev/null || true
    fi
    # The shared directory can be read by other accounts, and a lock taken
    # through a read-only descriptor would hold up every install: the lock
    # file is the user's alone, also one that an earlier start left open.
    (umask 077 && : >> "$lock") 2>/dev/null || true
    chmod 600 "$lock" 2>/dev/null || true
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

# Move a verified or freshly built file into place, record its origin $2,
# remove the programs of earlier launchers and start the server. The move is
# a rename in one directory: atomic, and a new file, so running copies of
# the old program keep their file (overwriting a signed program in place
# would invalidate the signature of running copies).
install_and_run() {
    local src=$1 origin=$2 f d have first=0
    shift 2
    chmod "$prog_mode" "$src" || die "cannot make $src executable"
    # The file must be the program it is named after: at least the wanted
    # version, by its own answer.
    have=$(program_version "$src")
    case "$(version_cmp "${have:-none}" "$version")" in
        0 | 1 ) ;;
        * ) die "the new program is not hxprobe $version (it reports '${have:-no version}'); it was deleted and not run" ;;
    esac
    # mv would put the file into a directory (or a link to one) of that name.
    [ ! -d "$bin" ] || die "$bin is a directory; remove it and start again"
    # Written before the move. A launcher killed between the two commands
    # leaves the new record next to the old program. For most pairs that
    # costs one more download or nothing (an older program is replaced at
    # the next start anyway). A `release` record next to another program
    # makes that program a changed one: the next start downloads the release
    # program again, and stops while that download fails. Written after the
    # move, a build that replaces a release program would leave the old
    # `release` record next to the built program, which no download repairs.
    printf '%s\n' "$origin" > "$origin_file" || die "cannot write $origin_file"
    if [ -n "$origin_mode" ]; then
        chmod "$origin_mode" "$origin_file" || die "cannot set the mode of $origin_file"
    fi
    if [ ! -e "$bin" ] && [ ! -L "$bin" ]; then first=1; fi
    if ! mv -f "$src" "$bin"; then
        rm -f "$origin_file" 2>/dev/null || true
        die "cannot move $src to $bin"
    fi
    tmp_file=''
    rm -f "$backoff_file" 2>/dev/null || true
    if [ -n "$legacy" ]; then
        for f in "$legacy"/hxprobe-*; do
            [ -f "$f" ] || continue
            rm -f "$f" || true
        done
        # They also built and locked in the plugin data directory. Not when
        # that is the install directory, under whatever spelling (-ef
        # compares the directories, not the text): its lock is the one held
        # here.
        d=${legacy%/bin}
        if ! [ "$d" -ef "$inst" ]; then
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
    # The program of earlier versions, which installed per user, is left
    # alone (other clients may still name it); said once, at the first
    # install into the shared directory.
    if [ "$in_shared" = 1 ] && [ "$first" = 1 ] && [ -n "$home" ]; then
        f="$home/Library/Application Support/hxprobe/bin/hxprobe"
        if [ -f "$f" ]; then
            log "the earlier install $f is no longer used by the plugin; remove it, and its Full Disk Access entry, when no client registration names it"
        fi
    fi
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
# $tmp_file. Returns 1 when no release program is listed for this machine and
# 2 when it could not be downloaded. A file that does not match its checksum
# is deleted and ends the launcher; when a built program of the wanted
# version is installed (`upgrade` is 1) the function returns 3 instead.
try_download() {
    local base url proto got
    release_sum || return 1
    command -v curl >/dev/null 2>&1 || { log "curl not found; cannot download $asset"; return 2; }
    command -v shasum >/dev/null 2>&1 || { log "shasum not found; cannot verify $asset"; return 2; }

    base=${HXPROBE_RELEASE_BASE:-https://github.com/oleggohman/outlook-mcp/releases/download}
    base=${base%/}
    case "$base" in
        file://* ) proto='=file' ;;
        * ) proto='=https' ;;
    esac
    url="$base/v$version/$asset"
    tmp_file=$(mktemp "$bindir/.download.XXXXXX") || return 2
    log "downloading $url"
    if ! run_child curl -fsSL --proto "$proto" --proto-redir "$proto" --tlsv1.2 \
            --connect-timeout 15 --max-time 300 -o "$tmp_file" "$url"; then
        log "download of $asset failed"
        rm -f "$tmp_file"; tmp_file=''
        return 2
    fi
    got=$(shasum -a 256 "$tmp_file" | awk '{ print tolower($1) }')
    if [ "$got" != "$want" ]; then
        rm -f "$tmp_file"; tmp_file=''
        if [ "$upgrade" = 1 ]; then
            log "checksum mismatch for $asset (expected $want, got $got); the file was deleted and not run. Check release/SHA256SUMS and the release."
            return 3
        fi
        die "checksum mismatch for $asset (expected $want, got $got); the file was deleted and not run, and nothing is built or run in its place. Check release/SHA256SUMS and the release, or set HXPROBE_BIN."
    fi
    log "checksum of $asset verified"
}

if [ "$upgrade" = 1 ]; then
    log "hxprobe $version at $bin was built here and a release program is listed; replacing it"
fi
if [ "$changed" = 1 ]; then
    log "the release program at $bin does not match the checksum it was installed with; it changed since the install and is not run. Replacing it"
fi
# The install itself runs outside the `if` condition, where set -e applies.
downloaded=0
try_download || downloaded=$?
if [ "$downloaded" = 0 ]; then
    install_and_run "$tmp_file" "release $want" ${1+"$@"}
fi
# A changed release file is never run, also not when the download failed.
# Without a release program for this version (status 1) it is replaced by a
# build below.
[ "$changed" = 0 ] || [ "$downloaded" = 1 ] || die "the installed release program at $bin does not match the checksum it was installed with and the release program could not be downloaded again; the changed file was not run. Remove it and start again, or set HXPROBE_BIN."
if [ "$downloaded" = 2 ] || [ "$upgrade" = 1 ]; then
    # Not a reason to build while a program that answers is installed. With
    # `upgrade` this is the built program of the wanted version.
    have=$(installed_version)
    if [ -n "$have" ]; then
        if [ "$upgrade" = 1 ]; then
            # Only read for a built program of the wanted version.
            touch "$backoff_file" 2>/dev/null || true
            log "running the installed hxprobe $have at $bin; the download is tried again in about 6 hours"
        else
            log "running the installed hxprobe $have at $bin; the next start tries the download again"
        fi
        release_lock
        run_server "$bin" ${1+"$@"}
    fi
fi
# A file that did not match its checksum is never answered with a build.
[ "$downloaded" != 3 ] || die "the installed hxprobe $version at $bin no longer answers and the release file did not match its checksum; nothing is built in its place. Check release/SHA256SUMS and the release, or set HXPROBE_BIN."

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
    install_and_run "$tmp_file" build ${1+"$@"}
fi

# f. Nothing worked.
why=''
if [ "$changed" = 1 ]; then
    why="the installed program was not run because it changed since its install; "
fi
die "${why}no prebuilt hxprobe $version is available for this machine and cargo is not on PATH. Install Rust from https://rustup.rs and restart the MCP server, or set HXPROBE_BIN to the path of an hxprobe program."
