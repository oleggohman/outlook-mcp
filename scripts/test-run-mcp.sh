#!/bin/bash
# Tests for scripts/run-mcp.sh. No network: downloads come from file:// URLs,
# and the program is a small shell script that prints its arguments.
# Each case runs against a temporary copy of the repository layout, so the
# real release/SHA256SUMS is never touched, and with HOME, the install
# directory (HXPROBE_INSTALL_DIR), the shared directory (HXPROBE_SHARED_DIR)
# and the plugin data directory (CLAUDE_PLUGIN_DATA) in a temporary
# directory. Every start of the launcher goes through `guard_shared`, so no
# case can reach the real shared directory.

set -euo pipefail

here=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
launcher="$here/run-mcp.sh"
version=9.8.7

root=$(mktemp -d "${TMPDIR:-/tmp}/test-run-mcp.XXXXXX")
trap 'rm -rf "$root"' EXIT

# The real shared directory of the launcher. The tests must leave it as they
# found it (absent, or the same inode and times); checked at the end.
real_shared=$(sed -n 's/^shared_default=//p' "$launcher")
[ -n "$real_shared" ] || real_shared=/Users/Shared/hxprobe
real_state() { stat -f '%i %m %c' "$real_shared" 2>/dev/null || echo none; }
real_before=$(real_state)

# The shared directory of the case in repo $1: inside the case, with a space
# in the path (the real default has none, the fallback under HOME has one).
shared_of() { printf '%s\n' "$1/Shared Folder/hxprobe"; }

# Stops the whole test unless $1, the HXPROBE_SHARED_DIR a launcher is about
# to get, lies inside the test's temp directory.
guard_shared() {
    case "$1" in
        "$root"/?* ) ;;
        * ) printf 'FATAL: HXPROBE_SHARED_DIR=%s is not inside %s; no launcher is started\n' "$1" "$root"
            exit 2 ;;
    esac
}

case "$(uname -m)" in
    arm64 | aarch64 ) arch=arm64 ;;
    *) arch=x86_64 ;;
esac
asset="hxprobe-$version-macos-$arch"
# PATH without cargo (rustup installs it outside the system directories).
nocargo_path=/usr/bin:/bin:/usr/sbin:/sbin

failures=0
pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s: %s\n' "$1" "$2"; failures=$((failures + 1)); }

# Shell code for the stubs: sets f3 and f9 to `open` or `closed`, the state of
# fd 3 (the parked stdout of the launcher) and fd 9 (the install lock).
fd_probe='f3=closed; f9=closed
if ( : >&3 ) 2>/dev/null; then f3=open; fi
if ( : >&9 ) 2>/dev/null; then f9=open; fi'
fds_closed='fd3=closed fd9=closed'

# Fake program: answers `--version` with `hxprobe $4` (without $4 it fails
# with a usage message and status 2, as 0.1.0 does) and leaves the file
# `<marker>.asked` when it is asked that. Otherwise it prints its
# arguments to stdout and leaves a marker file that holds the state of fd 3
# and fd 9, the file `<marker>.path` with the path it was run as and the file
# `<marker>.note` with HXPROBE_INSTALL_NOTE (`(unset)` when it is not set). $3
# is optional shell code to run last.
make_fake() {
    # $1 = path, $2 = marker file, $3 = shell code, $4 = version
    cat > "$1" <<EOF
#!/bin/sh
if [ "\$1" = --version ]; then
    : > "$2.asked"
    if [ -n "${4:-}" ]; then echo "hxprobe ${4:-}"; exit 0; fi
    echo "usage: hxprobe <command>" >&2
    exit 2
fi
echo "FAKE \$*"
$fd_probe
echo "fd3=\$f3 fd9=\$f9" > "$2"
printf '%s\n' "\$0" > "$2.path"
if [ -n "\${HXPROBE_INSTALL_NOTE+x}" ]; then printf '%s\n' "\$HXPROBE_INSTALL_NOTE"; else echo '(unset)'; fi > "$2.note"
${3:-}
EOF
    chmod 755 "$1"
}

# The inode number of file $1, or `none`.
inode() { stat -f %i "$1" 2>/dev/null || echo none; }

# The permission bits of $1, or `none`.
mode_of() { stat -f %Lp "$1" 2>/dev/null || echo none; }

# A release asset of the wanted version in repo $1 (served from file://$1/rel)
# that writes marker $1/ran, and its checksum line in release/SHA256SUMS.
make_asset() {
    mkdir -p "$1/rel/v$version"
    make_fake "$1/rel/v$version/$asset" "$1/ran" '' "$version"
    printf '%s  %s\n' "$(shasum -a 256 "$1/rel/v$version/$asset" | awk '{print $1}')" "$asset" \
        > "$1/release/SHA256SUMS"
}

# A curl stub in $1/stub that appends its arguments to $1/curl-args, records
# whether the install lock of $1/inst is held (`held` or `free`) in
# $1/curl-lock and then runs the real curl.
make_curl_probe() {
    mkdir -p "$1/stub"
    cat > "$1/stub/curl" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$1/curl-args"
if lockf -k -s -t 0 "$1/inst/install.lock" true 2>/dev/null; then echo free; else echo held; fi >> "$1/curl-lock"
exec /usr/bin/curl "\$@"
EOF
    chmod 755 "$1/stub/curl"
}

# A fresh repository layout: scripts/run-mcp.sh, hxprobe/Cargo.toml and an
# empty release/SHA256SUMS. Prints its path.
make_repo() {
    local r
    r=$(mktemp -d "$root/repo.XXXXXX")
    mkdir -p "$r/scripts" "$r/hxprobe" "$r/release" "$(dirname "$(shared_of "$r")")"
    # The parent of the shared directory, like /Users/Shared: the user's own,
    # not writable by others.
    chmod 755 "$(dirname "$(shared_of "$r")")"
    cp "$launcher" "$r/scripts/run-mcp.sh"
    cat > "$r/hxprobe/Cargo.toml" <<EOF
[package]
name = "hxprobe"
version = "$version"
edition = "2021"

[dependencies]
dep = { version = "1" }
EOF
    printf '# comment\n\n' > "$r/release/SHA256SUMS"
    printf '%s\n' "$r"
}

# Runs the launcher of repo $1 with the remaining arguments as `env` settings
# followed by `--` and launcher arguments. Sets out, err, rc. The launcher
# starts with fd 9 open, as it would under a parent that leaks one. The
# install directory is $1/inst, the shared directory `shared_of $1` and the
# plugin data directory $1/data unless the settings say otherwise (an empty
# value selects the default; HXPROBE_SHARED_DIR must stay inside the test's
# temp directory). The setting NOHOME runs it without HOME in the
# environment.
run() {
    local r=$1
    shift
    local envs='' home="$root/home" nohome='' shared
    shared=$(shared_of "$r")
    guard_shared "$shared"
    while [ "$1" != -- ]; do
        case "$1" in
            NOHOME ) nohome='-u HOME'; home='' ;;
            # Kept apart from the other settings: its value may hold a space.
            HXPROBE_SHARED_DIR=* ) shared=${1#HXPROBE_SHARED_DIR=}; guard_shared "$shared" ;;
            * ) envs="$envs $1" ;;
        esac
        shift
    done
    shift
    rc=0
    # shellcheck disable=SC2086
    env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS -u CLAUDE_PLUGIN_DATA \
        -u HXPROBE_INSTALL_DIR -u HXPROBE_SHARED_DIR -u HXPROBE_INSTALL_NOTE $nohome \
        ${home:+HOME="$home"} HXPROBE_INSTALL_DIR="$r/inst" HXPROBE_SHARED_DIR="$shared" \
        CLAUDE_PLUGIN_DATA="$r/data" $envs \
        bash "$r/scripts/run-mcp.sh" "$@" > "$root/out" 2> "$root/err" 9> "$root/fd9" || rc=$?
    out=$(cat "$root/out")
    err=$(cat "$root/err")
}

# Succeeds when nobody holds the install lock of install directory $1.
lock_free() {
    [ ! -e "$1/install.lock" ] || lockf -k -s -t 0 "$1/install.lock" true
}

# Counts leftovers of an install in install directory $1: temp files and
# build directories.
leftovers() {
    find "$1" -mindepth 1 \( -name '.download.*' -o -name '.build.*' -o -name 'target' -o -name 'target.*' \) | wc -l | tr -d ' '
}

# The recorded origin of the program in install directory $1, or `none`.
origin_of() { cat "$1/bin/.origin" 2>/dev/null || echo none; }

# A curl stub in $1/stub that records its arguments in $1/curl-args and
# fails as curl does for a download that cannot be made.
make_curl_fail() {
    mkdir -p "$1/stub"
    printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "%s/curl-args"\nexit 22\n' "$1" > "$1/stub/curl"
    chmod 755 "$1/stub/curl"
}

# An ls stub in $1/stub that shows uid $2 as the owner (third column of the
# first line) of every path at or under the shared directory of repo $1, and
# runs /bin/ls unchanged for everything else. It simulates a directory of
# another account (or of root, uid 0) without one.
make_ls_owner() {
    local s
    s=$(shared_of "$1")
    mkdir -p "$1/stub"
    cat > "$1/stub/ls" <<EOF
#!/bin/sh
for a; do p=\$a; done
case "\$p" in
    '$s' | '$s'/* )
        out=\$(/bin/ls "\$@") || exit \$?
        printf '%s\n' "\$out" | awk -v u='$2' 'NR == 1 { \$3 = u } { print }' ;;
    * ) exec /bin/ls "\$@" ;;
esac
EOF
    chmod 755 "$1/stub/ls"
}

# The pid of a process that has ended.
dead_pid() {
    sh -c 'exit 0' & local p=$!
    wait "$p"
    printf '%s\n' "$p"
}

# Succeeds when the process with pid $1 is gone, within about 5 s.
gone() {
    local i=0
    while kill -0 "$1" 2>/dev/null; do
        [ "$i" -lt 50 ] || return 1
        sleep 0.1
        i=$((i + 1))
    done
}

# A cargo stub in $1/stub: appends a line to $1/builds and the state of fd 3
# and fd 9 to $1/cargo-fds, copies its stdin to $1/cargo-stdin, runs the
# shell code $2 and leaves a program where cargo would.
make_cargo_stub() {
    mkdir -p "$1/stub"
    cat > "$1/stub/cargo" <<EOF
#!/bin/sh
echo "cargo noise on stdout"
echo start >> "$1/builds"
$fd_probe
echo "fd3=\$f3 fd9=\$f9" >> "$1/cargo-fds"
cat >> "$1/cargo-stdin"
$2
# Locate --target-dir and leave a program where cargo would.
while [ \$# -gt 0 ]; do
    if [ "\$1" = --target-dir ]; then t=\$2; fi
    shift
done
mkdir -p "\$t/release"
cat > "\$t/release/hxprobe" <<'PROG'
#!/bin/sh
if [ "\$1" = --version ]; then echo "hxprobe $version"; exit 0; fi
echo "BUILT \$*"
PROG
chmod 755 "\$t/release/hxprobe"
EOF
    chmod 755 "$1/stub/cargo"
}

# Starts $2 launchers of repo $1 at once with PATH $3 and waits for them.
# The install directory is $1/inst, or $4 when it is given (empty selects
# the default). Sets bad to the number that failed or printed something else
# than the server output.
run_many() {
    local r=$1 n=$2 path=$3 inst=${4-$1/inst} i pids='' p shared
    shared=$(shared_of "$r")
    guard_shared "$shared"
    bad=0
    i=0
    while [ "$i" -lt "$n" ]; do
        env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS -u HXPROBE_SHARED_DIR \
            HOME="$root/home" HXPROBE_INSTALL_DIR="$inst" CLAUDE_PLUGIN_DATA="$r/data" \
            HXPROBE_SHARED_DIR="$shared" \
            PATH="$path" bash "$r/scripts/run-mcp.sh" > "$r/out.$i" 2> "$r/err.$i" &
        pids="$pids $!"
        i=$((i + 1))
    done
    i=0
    for p in $pids; do
        if ! wait "$p" || [ "$(cat "$r/out.$i")" != 'BUILT mcp' ]; then bad=$((bad + 1)); fi
        i=$((i + 1))
    done
}

# (i) HXPROBE_BIN runs the given program with `mcp` and the arguments, and
# installs nothing.
t='HXPROBE_BIN'
r=$(make_repo)
make_fake "$r/fake" "$r/ran"
run "$r" HXPROBE_BIN="$r/fake" -- --store /x
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp --store /x' ] && [ -z "$err" ] \
    && [ "$(cat "$r/ran")" = "$fds_closed" ] && [ "$(cat "$r/ran.path")" = "$r/fake" ] \
    && [ ! -e "$r/inst" ] && [ ! -e "$r/data" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err' fds='$(cat "$r/ran" 2>/dev/null || true)'"; fi

# (i-b) HXPROBE_BIN that is not executable is an error, not a fallback.
t='HXPROBE_BIN not executable'
run "$r" HXPROBE_BIN="$r/missing" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == 'hxprobe plugin: HXPROBE_BIN is set'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (ii) An installed release program that reports the wanted version is run
# from the stable path without a download, without arguments too (bash 3.2
# with set -u and an empty "$@"). Legacy programs stay: nothing was installed.
t='installed program of the version'
r=$(make_repo)
make_asset "$r"
make_curl_probe "$r"
mkdir -p "$r/inst/bin" "$r/data/bin"
make_fake "$r/inst/bin/hxprobe" "$r/inst-ran" '' "$version"
echo "release $(shasum -a 256 "$r/inst/bin/hxprobe" | awk '{ print $1 }')" > "$r/inst/bin/.origin"
make_fake "$r/data/bin/hxprobe-0.1.0" "$r/old-ran"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -z "$err" ] \
    && [ "$(cat "$r/inst-ran")" = "$fds_closed" ] \
    && [ "$(cat "$r/inst-ran.path")" = "$r/inst/bin/hxprobe" ] \
    && [ ! -e "$r/curl-args" ] && [ -e "$r/data/bin/hxprobe-0.1.0" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err' curl='$(cat "$r/curl-args" 2>/dev/null || true)'"; fi

# (ii-b) Default install directory: the shared directory (HXPROBE_SHARED_DIR
# stands in for /Users/Shared/hxprobe). The program is installed as
# <shared>/bin/hxprobe and run from there, and nothing is created under HOME.
# The directories and the program have mode 755 and bin/.origin 644, also
# when the launcher starts with umask 077.
t='default install dir'
r=$(make_repo)
make_asset "$r"
d=$(shared_of "$r")
rm -rf "$root/home"
old_umask=$(umask)
umask 077
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" -- a b
umask "$old_umask"
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp a b' ] && [ "$(cat "$r/ran.path")" = "$d/bin/hxprobe" ] \
    && [ "$(mode_of "$d")" = 755 ] && [ "$(mode_of "$d/bin")" = 755 ] \
    && [ "$(mode_of "$d/bin/hxprobe")" = 755 ] && [ "$(mode_of "$d/bin/.origin")" = 644 ] \
    && cmp -s "$d/bin/hxprobe" "$r/rel/v$version/$asset" \
    && [[ "$(origin_of "$d")" == 'release '* ]] && [[ "$err" != *'was not used'* ]] \
    && [ ! -e "$root/home" ] && [ ! -e "$r/inst" ] && lock_free "$d"; then pass "$t"
else fail "$t" "rc=$rc out='$out' ran='$(cat "$r/ran.path" 2>/dev/null || true)' modes=$(mode_of "$d")/$(mode_of "$d/bin")/$(mode_of "$d/bin/hxprobe")/$(mode_of "$d/bin/.origin") err='$err'"; fi
rm -rf "$root/home"

# (ii-c) The install lock in the shared directory, which other accounts can
# read, is closed to them (mode 600): a lock taken through a read-only
# descriptor would hold up every install. A new lock file is made that way
# under umask 022, and one that an earlier start left open is closed.
for how in new 'left with mode 644'; do
    t="install lock closed to other accounts: $how"
    r=$(make_repo)
    make_asset "$r"
    d=$(shared_of "$r")
    rm -rf "$root/home"
    if [ "$how" != new ]; then
        mkdir -m 755 "$d"
        : > "$d/install.lock"
        chmod 644 "$d/install.lock"
    fi
    old_umask=$(umask)
    umask 022
    run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
    umask "$old_umask"
    if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(cat "$r/ran.path")" = "$d/bin/hxprobe" ] \
        && [ "$(mode_of "$d/install.lock")" = 600 ] && [ "$(mode_of "$d/bin/hxprobe")" = 755 ] \
        && lock_free "$d"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' lock mode=$(mode_of "$d/install.lock") err='$err'"; fi
done
rm -rf "$root/home"

# (iii) Download with a matching checksum: installed as <install dir>/bin/hxprobe
# with mode 0700 in directories of mode 0700, legacy versioned programs in
# <plugin data>/bin removed (other files stay), no temp files left, server
# started from the stable path.
t='download, checksum match'
r=$(make_repo)
mkdir -p "$r/rel/v$version" "$r/data/bin"
make_fake "$r/rel/v$version/$asset" "$r/ran" '' "$version"
sum=$(shasum -a 256 "$r/rel/v$version/$asset" | awk '{print $1}')
printf '# test sums\n%s  %s\n' "$sum" "$asset" > "$r/sums"
make_fake "$r/data/bin/hxprobe-0.0.1" "$r/old-ran"
make_fake "$r/data/bin/hxprobe-$version" "$r/old-ran"
: > "$r/data/bin/notes"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" \
    HXPROBE_SUMS="$r/sums" PATH="$nocargo_path" -- --refresh-secs 0
b="$r/inst/bin/hxprobe"
left=$({ find "$r/inst" "$r/data" -mindepth 1 \( -name '.download.*' -o -name 'hxprobe-*' \) \
    2>/dev/null || true; } | wc -l | tr -d ' ')
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp --refresh-secs 0' ] && [ "$(mode_of "$b")" = 700 ] \
    && [ "$(mode_of "$r/inst")" = 700 ] && [ "$(mode_of "$r/inst/bin")" = 700 ] \
    && [ "$(cat "$r/ran.path")" = "$b" ] && cmp -s "$b" "$r/rel/v$version/$asset" \
    && [ "$left" = 0 ] && [ -e "$r/data/bin/notes" ] && lock_free "$r/inst" \
    && [ "$(origin_of "$r/inst")" = "release $sum" ] \
    && [[ "$err" == *'checksum of '"$asset"' verified'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' mode=$(mode_of "$b") left=$left err='$err'"; fi

# (iii-b) A non-file base URL is limited to https: http:// is refused by curl
# before any connection, so the launcher falls through to the final error.
# The message must be curl's protocol error (status 1): nothing listens on
# the port, so a connection would fail too (status 7), for another reason.
t='download refuses http'
r=$(make_repo)
printf '%s  %s\n' "$sum" "$asset" > "$r/release/SHA256SUMS"
run "$r" HXPROBE_RELEASE_BASE="http://127.0.0.1:9" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *"download of $asset failed"* ]] \
    && [[ "$err" == *'curl: (1) '* ]] && [[ "$err" != *'curl: (7) '* ]] \
    && [ ! -e "$r/inst/bin/hxprobe" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (iii-c) The options that curl is given, recorded by a stub: https only
# (also for redirects), TLS 1.2 or later and time limits; `file` only for a
# file:// base URL. The stub also records that it has neither the lock (fd 9)
# nor the real stdout (fd 3).
t='curl options'
r=$(make_repo)
printf '%s  %s\n' "$sum" "$asset" > "$r/release/SHA256SUMS"
mkdir -p "$r/stub"
cat > "$r/stub/curl" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$r/curl-args"
$fd_probe
echo "fd3=\$f3 fd9=\$f9" >> "$r/curl-fds"
exit 22
EOF
chmod 755 "$r/stub/curl"
run "$r" PATH="$r/stub:$nocargo_path" --
run "$r" HXPROBE_RELEASE_BASE="http://127.0.0.1:9" PATH="$r/stub:$nocargo_path" --
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
a1=$(sed -n 1p "$r/curl-args" 2>/dev/null || true)
a2=$(sed -n 2p "$r/curl-args" 2>/dev/null || true)
a3=$(sed -n 3p "$r/curl-args" 2>/dev/null || true)
curl_ok=1
for a in "$a1" "$a2" "$a3"; do
    [[ " $a " == *' --tlsv1.2 '* ]] && [[ " $a " == *' --connect-timeout 15 '* ]] \
        && [[ " $a " == *' --max-time 300 '* ]] || curl_ok=0
done
for a in "$a1" "$a2"; do
    [[ " $a " == *' --proto =https --proto-redir =https '* ]] && [[ "$a" != *'=file'* ]] || curl_ok=0
done
[[ " $a3 " == *' --proto =file --proto-redir =file '* ]] && [[ "$a3" != *'=https'* ]] || curl_ok=0
if [ "$curl_ok" = 1 ] && [[ "$a1" == *" https://github.com/"*"/v$version/$asset" ]] \
    && [[ "$a2" == *" http://127.0.0.1:9/v$version/$asset" ]] \
    && [ "$(sort -u "$r/curl-fds")" = "$fds_closed" ] \
    && [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *"download of $asset failed"* ]]; then pass "$t"
else fail "$t" "rc=$rc a1='$a1' a2='$a2' a3='$a3' fds='$(cat "$r/curl-fds" 2>/dev/null || true)' err='$err'"; fi

# (iii-d) A failed move into place stops the install: nothing is reported as
# installed and no temp file is left.
t='install failure stops'
r=$(make_repo)
make_asset "$r"
mkdir -p "$r/stub"
printf '#!/bin/sh\necho "mv: stub failure" >&2\nexit 1\n' > "$r/stub/mv"
chmod 755 "$r/stub/mv"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/ran" ] && [[ "$err" == *'cannot move'* ]] \
    && [[ "$err" != *'installed hxprobe'* ]] && [ ! -e "$r/inst/bin/hxprobe" ] \
    && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (iv) Checksum mismatch: the file is not run and not left behind.
t='download, checksum mismatch'
r=$(make_repo)
make_asset "$r"
printf '%s  %s\n' "$(printf '0%.0s' $(seq 64))" "$asset" > "$r/release/SHA256SUMS"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
files=$({ find "$r/inst/bin" -mindepth 1 2>/dev/null || true; } | wc -l | tr -d ' ')
if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/ran" ] && [ "$files" = 0 ] \
    && [[ "$err" == *'checksum mismatch'* ]] && lock_free "$r/inst"; then pass "$t"
else fail "$t" "rc=$rc out='$out' files=$files err='$err'"; fi

# (iv-b) Checksum mismatch with an older program installed: that program is
# left untouched (same file, same content) and neither file is run.
t='checksum mismatch keeps the installed program'
r=$(make_repo)
make_asset "$r"
printf '%s  %s\n' "$(printf '0%.0s' $(seq 64))" "$asset" > "$r/release/SHA256SUMS"
mkdir -p "$r/inst/bin"
make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' 9.8.6
cp "$r/inst/bin/hxprobe" "$r/old-copy"
ino=$(inode "$r/inst/bin/hxprobe")
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/ran" ] && [ ! -e "$r/old-ran" ] \
    && [ "$(inode "$r/inst/bin/hxprobe")" = "$ino" ] && cmp -s "$r/inst/bin/hxprobe" "$r/old-copy" \
    && [ "$(leftovers "$r/inst")" = 0 ] && [[ "$err" == *'checksum mismatch'* ]] \
    && lock_free "$r/inst"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (iv-c) An older installed program is replaced by a rename under the
# install lock: a new file (new inode) with the asset's bytes, no temp file
# left, the new program run. The same for a program that gives no answer to
# --version (0.1.0 prints its usage and exits 2).
for kind in older 'without --version'; do
    t="$kind program replaced"
    r=$(make_repo)
    make_asset "$r"
    make_curl_probe "$r"
    mkdir -p "$r/inst/bin"
    if [ "$kind" = older ]; then have=9.8.6; else have=''; fi
    make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' "$have"
    ino=$(inode "$r/inst/bin/hxprobe")
    run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    b="$r/inst/bin/hxprobe"
    if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ ! -e "$r/old-ran" ] \
        && [ "$(cat "$r/ran.path")" = "$b" ] && [ "$(inode "$b")" != "$ino" ] \
        && cmp -s "$b" "$r/rel/v$version/$asset" && [ "$(mode_of "$b")" = 700 ] \
        && [ "$(cat "$r/curl-lock")" = held ] && [ "$(leftovers "$r/inst")" = 0 ] \
        && [ "$(find "$r/inst/bin" -mindepth 1 | wc -l | tr -d ' ')" = 2 ] \
        && [[ "$(origin_of "$r/inst")" == 'release '* ]] \
        && lock_free "$r/inst"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' lock='$(cat "$r/curl-lock" 2>/dev/null || true)' err='$err'"; fi
done

# (iv-d) A newer installed program is run as it is, with a log line: never
# downgrade. 9.10.0 is newer than 9.8.7 only when compared by numbers.
t='newer program kept'
r=$(make_repo)
make_asset "$r"
make_curl_probe "$r"
mkdir -p "$r/inst/bin"
make_fake "$r/inst/bin/hxprobe" "$r/new-ran" '' 9.10.0
ino=$(inode "$r/inst/bin/hxprobe")
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -e "$r/new-ran" ] && [ ! -e "$r/ran" ] \
    && [ "$(inode "$r/inst/bin/hxprobe")" = "$ino" ] && [ ! -e "$r/curl-args" ] \
    && [[ "$err" == *'9.10.0'*'newer'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (v) No cargo and no asset: exit 1 with the fix.
t='no cargo, no asset'
r=$(make_repo)
run "$r" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *'https://rustup.rs'* ]] \
    && [[ "$err" == *HXPROBE_BIN* ]] && lock_free "$r/inst"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (v-b) Missing version in Cargo.toml: clear error.
t='no version'
r=$(make_repo)
printf '[package]\nname = "hxprobe"\n[dependencies]\nversion = "1"\n' > "$r/hxprobe/Cargo.toml"
run "$r" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *'no version found'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (vi) The launcher itself writes nothing to stdout: with a cargo stub that
# prints to stdout, and with a download, stdout holds only the server output.
# The cargo stub has neither the lock (fd 9) nor the real stdout (fd 3), and
# reads nothing from the stdin of the launcher, which belongs to the server.
t='stdout clean'
r=$(make_repo)
make_cargo_stub "$r" ''
printf 'request\n' > "$r/stdin"
run "$r" PATH="$r/stub:$nocargo_path" -- x < "$r/stdin"
built_ok=0
if [ "$rc" = 0 ] && [ "$out" = 'BUILT mcp x' ] && [[ "$err" == *'cargo noise on stdout'* ]] \
    && [ "$(leftovers "$r/inst")" = 0 ] && [ -x "$r/inst/bin/hxprobe" ] \
    && [ "$(cat "$r/cargo-fds")" = "$fds_closed" ] && [ -e "$r/cargo-stdin" ] \
    && [ ! -s "$r/cargo-stdin" ]; then built_ok=1; fi
r2=$(make_repo)
make_asset "$r2"
run "$r2" HXPROBE_RELEASE_BASE="file://$r2/rel" PATH="$nocargo_path" --
if [ "$built_ok" = 1 ] && [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ]; then pass "$t"
else fail "$t" "built_ok=$built_ok rc=$rc out='$out' cargo-fds='$(cat "$r/cargo-fds" 2>/dev/null || true)' err='$err'"; fi

# (vi-b) The cargo build is installed at the same stable path, replacing an
# older program by a rename, and the launcher notes that a Full Disk Access
# grant for a program built here may not survive the next build. Legacy
# programs are removed after the install.
t='cargo build at the stable path'
r=$(make_repo)
make_cargo_stub "$r" ''
mkdir -p "$r/inst/bin" "$r/data/bin"
make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' 9.8.6
make_fake "$r/data/bin/hxprobe-0.1.0" "$r/old-ran"
ino=$(inode "$r/inst/bin/hxprobe")
run "$r" PATH="$r/stub:$nocargo_path" --
b="$r/inst/bin/hxprobe"
if [ "$rc" = 0 ] && [ "$out" = 'BUILT mcp' ] && [ "$(mode_of "$b")" = 700 ] \
    && [ "$(inode "$b")" != "$ino" ] && [ ! -e "$r/old-ran" ] \
    && [ ! -e "$r/data/bin/hxprobe-0.1.0" ] && [ "$(leftovers "$r/inst")" = 0 ] \
    && [ "$(printf '%s\n' "$err" | grep -c 'Full Disk Access')" = 1 ] \
    && [ "$(origin_of "$r/inst")" = build ] && lock_free "$r/inst"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (vii) The install lock. A lock directory left by an earlier launcher is
# replaced by the lock file; a held lock is waited for until the holder has
# installed the program.
t='old lock directory'
r=$(make_repo)
mkdir -p "$r/inst/install.lock" "$r/inst/bin"
dead_pid > "$r/inst/install.lock/pid"
run "$r" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [[ "$err" == *'https://rustup.rs'* ]] && [[ "$err" != *'without a lock'* ]] \
    && [ -f "$r/inst/install.lock" ] && lock_free "$r/inst"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# A link in the place of the lock file is not followed: the file it names
# keeps its mode and is not opened.
t='lock file is a link'
r=$(make_repo)
mkdir -p "$r/inst"
: > "$r/elsewhere"
chmod 644 "$r/elsewhere"
ln -s "$r/elsewhere" "$r/inst/install.lock"
run "$r" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *"$r/inst/install.lock is a symbolic link"* ]] \
    && [[ "$err" != *'https://rustup.rs'* ]] && [ -L "$r/inst/install.lock" ] \
    && [ "$(mode_of "$r/elsewhere")" = 644 ] && [ ! -s "$r/elsewhere" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' target mode=$(mode_of "$r/elsewhere") err='$err'"; fi

t='live lock'
r=$(make_repo)
mkdir -p "$r/inst/bin"
make_fake "$r/fake" "$r/ran" '' "$version"
lockf -k "$r/inst/install.lock" sh -c "sleep 2; cp '$r/fake' '$r/inst/bin/hxprobe'" &
helper=$!
i=0
while lock_free "$r/inst" && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
run "$r" PATH="$nocargo_path" --
wait "$helper" || true
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [[ "$err" == *'waiting'* ]] \
    && lock_free "$r/inst"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (viii) Many launchers at once: exactly one build and one install run and
# every launcher starts the server. Second variant: the lock file and build
# directories are left over from a process that was killed while it held the
# lock.
many=8
rounds=5
for variant in 'fresh' 'leftover lock of a dead process'; do
    t="concurrent starts, $variant"
    detail=''
    round=0
    while [ "$round" -lt "$rounds" ] && [ -z "$detail" ]; do
        round=$((round + 1))
        r=$(make_repo)
        make_cargo_stub "$r" 'sleep 0.3'
        if [ "$variant" != fresh ]; then
            mkdir -p "$r/inst/bin" "$r/inst/target.$(dead_pid)/release"
            # The holder waits for the lock: lock_free below holds it for
            # a moment, and a holder that tried only once could miss it.
            ( exec 9>> "$r/inst/install.lock"; lockf -s -t 10 9; exec sleep 30 ) &
            holder=$!
            i=0
            while lock_free "$r/inst" && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
            kill -9 "$holder"
            wait "$holder" 2>/dev/null || true
        fi
        run_many "$r" "$many" "$r/stub:$nocargo_path"
        builds=$(wc -l < "$r/builds" | tr -d ' ')
        installs=$(cat "$r"/err.* | grep -c 'installed hxprobe' || true)
        if [ "$bad" != 0 ] || [ "$builds" != 1 ] || [ "$installs" != 1 ] \
            || [ "$(leftovers "$r/inst")" != 0 ] || [ ! -x "$r/inst/bin/hxprobe" ] \
            || ! lock_free "$r/inst"; then
            detail="round $round: bad=$bad builds=$builds installs=$installs leftovers=$(leftovers "$r/inst") err='$(cat "$r"/err.*)'"
        fi
    done
    if [ -z "$detail" ]; then pass "$t"; else fail "$t" "$detail"; fi
done

# (viii-b) Without lockf and flock the install runs unlocked: several builds
# may run, each in its own directory, and every launcher still starts the
# server.
t='concurrent starts, no lock tool'
r=$(make_repo)
make_cargo_stub "$r" 'sleep 0.3'
mkdir -p "$r/tools"
for tool in dirname awk mktemp uname env; do ln -s "/usr/bin/$tool" "$r/tools/$tool"; done
run_many "$r" "$many" "$r/stub:$r/tools:/bin"
builds=$(wc -l < "$r/builds" | tr -d ' ')
if [ "$bad" = 0 ] && [ "$builds" -ge 1 ] && [ "$(leftovers "$r/inst")" = 0 ] \
    && [[ "$(cat "$r/err.0")" == *'installing without a lock'* ]]; then pass "$t"
else fail "$t" "bad=$bad builds=$builds leftovers=$(leftovers "$r/inst") err='$(cat "$r"/err.*)'"; fi

# (vii-b) The lock is free while the server that was just installed runs, and
# the server has neither fd 9 nor fd 3.
t='lock free while the server runs'
r=$(make_repo)
mkdir -p "$r/rel/v$version"
make_fake "$r/rel/v$version/$asset" "$r/ran" 'sleep 2' "$version"
printf '%s  %s\n' "$(shasum -a 256 "$r/rel/v$version/$asset" | awk '{print $1}')" "$asset" > "$r/release/SHA256SUMS"
guard_shared "$(shared_of "$r")"
env -u HXPROBE_BIN -u HXPROBE_SUMS -u HXPROBE_SHARED_DIR HOME="$root/home" HXPROBE_INSTALL_DIR="$r/inst" \
    HXPROBE_SHARED_DIR="$(shared_of "$r")" \
    CLAUDE_PLUGIN_DATA="$r/data" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" \
    bash "$r/scripts/run-mcp.sh" > "$r/out" 2> "$r/err" 9> "$root/fd9" &
pid=$!
i=0
while [ ! -e "$r/ran" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
running=0; free=0
if kill -0 "$pid" 2>/dev/null; then running=1; fi
if lock_free "$r/inst"; then free=1; fi
rc=0
wait "$pid" || rc=$?
if [ "$running" = 1 ] && [ "$free" = 1 ] && [ "$rc" = 0 ] && [ "$(cat "$r/out")" = 'FAKE mcp' ] \
    && [ "$(cat "$r/ran" 2>/dev/null || true)" = "$fds_closed" ]; then pass "$t"
else fail "$t" "running=$running free=$free rc=$rc fds='$(cat "$r/ran" 2>/dev/null || true)' err='$(cat "$r/err")'"; fi

# (ix) TERM during a build or a download ends the launcher at once (well
# before the stub's 30 s; the limits leave room for a loaded machine and for
# whole-second time stamps) with a non-zero status; the lock is free and
# nothing is left behind. The
# command and what it started are gone as well: each stub starts a second
# process, which during a build keeps writing into the build directory. In
# the third variant that process ignores TERM, so the launcher has to kill it
# (after 2 s).
for what in build download 'build, TERM ignored'; do
    t="TERM during $what"
    r=$(make_repo)
    mkdir -p "$r/stub"
    limit=10
    ignore=''
    if [ "$what" = 'build, TERM ignored' ]; then limit=15; ignore="trap '' TERM"; fi
    if [ "$what" = download ]; then
        printf '%s  %s\n' "$sum" "$asset" > "$r/release/SHA256SUMS"
        cat > "$r/stub/curl" <<EOF
#!/bin/sh
sleep 30 &
echo \$! > "$r/grandchild.pid"
echo \$\$ > "$r/stub.pid"
: > "$r/started"
sleep 30
EOF
    else
        cat > "$r/stub/cargo" <<EOF
#!/bin/sh
while [ \$# -gt 0 ]; do
    if [ "\$1" = --target-dir ]; then t=\$2; fi
    shift
done
(
    $ignore
    i=0
    while :; do
        mkdir -p "\$t/release"
        : > "\$t/release/f.\$i"
        i=\$((i + 1))
        sleep 0.05
    done
) &
echo \$! > "$r/grandchild.pid"
echo \$\$ > "$r/stub.pid"
: > "$r/started"
sleep 30
EOF
    fi
    chmod 755 "$r/stub"/*
    guard_shared "$(shared_of "$r")"
    env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS -u HXPROBE_SHARED_DIR \
        HOME="$root/home" HXPROBE_INSTALL_DIR="$r/inst" CLAUDE_PLUGIN_DATA="$r/data" \
        HXPROBE_SHARED_DIR="$(shared_of "$r")" PATH="$r/stub:$nocargo_path" \
        bash "$r/scripts/run-mcp.sh" > "$r/out" 2> "$r/err" &
    pid=$!
    i=0
    while [ ! -e "$r/started" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
    before=$(date +%s)
    kill -TERM "$pid"
    rc=0
    wait "$pid" || rc=$?
    took=$(( $(date +%s) - before ))
    stub_pid=$(cat "$r/stub.pid" 2>/dev/null || true)
    grandchild_pid=$(cat "$r/grandchild.pid" 2>/dev/null || true)
    alive=''
    for p in $stub_pid $grandchild_pid; do
        if ! gone "$p"; then alive="$alive $p"; fi
    done
    # A writer that survived would have put the build directory back by now.
    sleep 0.3
    if [ -e "$r/started" ] && [ "$rc" = 143 ] && [ "$took" -le "$limit" ] && [ ! -s "$r/out" ] \
        && [ -n "$stub_pid" ] && [ -n "$grandchild_pid" ] && [ -z "$alive" ] \
        && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst" \
        && [ ! -e "$r/inst/bin/hxprobe" ]; then pass "$t"
    else fail "$t" "rc=$rc took=${took}s alive='$alive' leftovers=$(leftovers "$r/inst") err='$(cat "$r/err")'"; fi
    for p in $stub_pid $grandchild_pid; do kill -KILL "$p" 2>/dev/null || true; done
done

# (ix-b) TERM while the launcher waits for the lock: it ends at once and the
# lockf process that was waiting is gone. A lockf stub records the pids.
t='TERM during the wait for the lock'
r=$(make_repo)
mkdir -p "$r/stub" "$r/inst/bin"
cat > "$r/stub/lockf" <<EOF
#!/bin/sh
echo \$\$ >> "$r/lockf.pids"
exec /usr/bin/lockf "\$@"
EOF
chmod 755 "$r/stub/lockf"
# The holder waits for the lock (see above).
( exec 9>> "$r/inst/install.lock"; lockf -s -t 10 9; exec sleep 30 ) &
holder=$!
i=0
while lock_free "$r/inst" && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
guard_shared "$(shared_of "$r")"
env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS -u HXPROBE_SHARED_DIR \
    HOME="$root/home" HXPROBE_INSTALL_DIR="$r/inst" CLAUDE_PLUGIN_DATA="$r/data" \
    HXPROBE_SHARED_DIR="$(shared_of "$r")" PATH="$r/stub:$nocargo_path" \
    bash "$r/scripts/run-mcp.sh" > "$r/out" 2> "$r/err" &
pid=$!
# Two lockf runs: the attempt that fails at once and the one that waits.
i=0
while [ "$(cat "$r/lockf.pids" 2>/dev/null | wc -l)" -lt 2 ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
# No pause is needed here: a signal that comes while the launcher starts the
# waiting lockf is acted on as soon as that is started.
before=$(date +%s)
kill -TERM "$pid"
rc=0
wait "$pid" || rc=$?
took=$(( $(date +%s) - before ))
waiters=$(cat "$r/lockf.pids" 2>/dev/null || true)
alive=''
for p in $waiters; do
    if ! gone "$p"; then alive="$alive $p"; fi
done
if [ "$(printf '%s\n' "$waiters" | wc -l | tr -d ' ')" = 2 ] && [ "$rc" = 143 ] && [ "$took" -le 10 ] \
    && [ ! -s "$r/out" ] && [ -z "$alive" ] && [[ "$(cat "$r/err")" == *'waiting'* ]] \
    && kill -0 "$holder" 2>/dev/null; then pass "$t"
else fail "$t" "rc=$rc took=${took}s waiters='$waiters' alive='$alive' err='$(cat "$r/err")'"; fi
for p in $waiters; do kill -KILL "$p" 2>/dev/null || true; done
kill -KILL "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true

# (x) HOME unset or empty. With an install directory given the launcher
# needs no HOME (the plugin data directory then has no default, so no legacy
# programs are looked for). Without one the shared directory is used, which
# needs no HOME either; when that cannot be used, the launcher stops and says
# what to set.
for how in unset empty; do
    t="HOME $how"
    if [ "$how" = unset ]; then nohome=NOHOME; else nohome='HOME='; fi
    r=$(make_repo)
    make_asset "$r"
    run "$r" "$nohome" CLAUDE_PLUGIN_DATA= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" -- a
    rc1=$rc; out1=$out; err1=$err
    r2=$(make_repo)
    make_asset "$r2"
    run "$r2" "$nohome" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r2/rel" PATH="$nocargo_path" --
    rc2=$rc; out2=$out; err2=$err
    r3=$(make_repo)
    make_asset "$r3"
    run "$r3" "$nohome" HXPROBE_INSTALL_DIR= HXPROBE_SHARED_DIR="$r3/missing/hxprobe" \
        HXPROBE_RELEASE_BASE="file://$r3/rel" PATH="$nocargo_path" --
    if [ "$rc1" = 0 ] && [ "$out1" = 'FAKE mcp a' ] && [ "$(cat "$r/ran.path")" = "$r/inst/bin/hxprobe" ] \
        && [[ "$err1" != *'unbound variable'* ]] \
        && [ "$rc2" = 0 ] && [ "$out2" = 'FAKE mcp' ] \
        && [ "$(cat "$r2/ran.path" 2>/dev/null || true)" = "$(shared_of "$r2")/bin/hxprobe" ] \
        && [[ "$err2" != *'unbound variable'* ]] \
        && [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r3/ran" ] && [ ! -e "$r3/missing" ] \
        && [[ "$err" == *'was not used'* ]] \
        && [[ "$err" == *'HOME is not set; set HXPROBE_INSTALL_DIR'* ]]; then pass "$t"
    else fail "$t" "rc=$rc1/$rc2/$rc out='$out1'/'$out2' err='$err1' / '$err2' / '$err'"; fi
done

# (xi) The install directory must belong to the user, and its mode must be
# settable: a program that runs from it reads the user's mail. The chmod stub
# changes nothing, so the directory of another owner is only looked at.
t='install dir of another owner'
r=$(make_repo)
make_asset "$r"
make_curl_probe "$r"
printf '#!/bin/sh\nexit 1\n' > "$r/stub/chmod"
chmod 755 "$r/stub/chmod"
if [ "$(id -u)" = 0 ]; then
    pass "$t (skipped for root)"
else
    run "$r" HXPROBE_INSTALL_DIR=/usr HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *'not owned by'* ]] \
        && [ ! -e "$r/curl-args" ] && [ ! -e "$r/ran" ]; then pass "$t"
    else fail "$t" "rc=$rc out='$out' err='$err'"; fi
fi

t='install dir mode cannot be set'
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *'cannot set the mode'* ]] \
    && [ ! -e "$r/curl-args" ] && [ ! -e "$r/ran" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (xii) Versions with a suffix. A pre-release (`-rc1`) is older than the
# release with the same numbers and is replaced by it; build metadata (`+b5`)
# makes no difference; a pre-release of a later version is newer.
for have in 9.8.7-rc1 9.8.7+b5 9.8.8-rc1; do
    t="installed $have"
    r=$(make_repo)
    make_asset "$r"
    make_curl_probe "$r"
    mkdir -p "$r/inst/bin"
    make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' "$have"
    echo "release $(shasum -a 256 "$r/inst/bin/hxprobe" | awk '{ print $1 }')" > "$r/inst/bin/.origin"
    run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    ok=0
    case "$have" in
        9.8.7-rc1 ) [ -e "$r/ran" ] && [ ! -e "$r/old-ran" ] && [ -e "$r/curl-args" ] && ok=1 ;;
        9.8.7+b5 ) [ -e "$r/old-ran" ] && [ ! -e "$r/curl-args" ] && [ -z "$err" ] && ok=1 ;;
        9.8.8-rc1 ) [ -e "$r/old-ran" ] && [ ! -e "$r/curl-args" ] && [[ "$err" == *'newer'* ]] && ok=1 ;;
    esac
    if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$ok" = 1 ]; then pass "$t"
    else fail "$t" "rc=$rc out='$out' err='$err'"; fi
done

# (xiii) A directory where the program belongs (or a link to one): the
# install stops instead of moving the program into it.
for kind in directory 'link to a directory'; do
    t="program path is a $kind"
    r=$(make_repo)
    make_asset "$r"
    mkdir -p "$r/inst/bin" "$r/elsewhere"
    if [ "$kind" = directory ]; then mkdir "$r/inst/bin/hxprobe"; else ln -s "$r/elsewhere" "$r/inst/bin/hxprobe"; fi
    run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
    inside=$(find "$r/inst/bin/hxprobe/" "$r/elsewhere" -mindepth 1 2>/dev/null | wc -l | tr -d ' ')
    if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/ran" ] && [ "$inside" = 0 ] \
        && [[ "$err" == *'is a directory'* ]] && [[ "$err" != *'installed hxprobe'* ]] \
        && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' inside=$inside err='$err'"; fi
done

# (xiv) Temp files that a killed launcher left in the bin directory are
# removed by the next install, which holds the lock.
t='temp files of a killed launcher'
r=$(make_repo)
make_asset "$r"
mkdir -p "$r/inst/bin"
: > "$r/inst/bin/.download.OLD001"
: > "$r/inst/bin/.build.OLD002"
: > "$r/inst/bin/notes"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(leftovers "$r/inst")" = 0 ] \
    && [ -e "$r/inst/bin/notes" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' leftovers=$(leftovers "$r/inst") err='$err'"; fi

# (xv) Build directories. The launcher builds in <install dir>/target.<pid>
# and never made <install dir>/target: a directory of that name is not its
# to remove. What an earlier launcher left in the plugin data directory
# (target, target.<pid>, install.lock) goes after a successful install;
# other files there stay.
t='build directories of earlier launchers'
r=$(make_repo)
make_asset "$r"
mkdir -p "$r/inst/target/keep" "$r/data/target/release" "$r/data/target.$(dead_pid)/release" "$r/data/bin"
: > "$r/data/install.lock"
: > "$r/data/notes"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -d "$r/inst/target/keep" ] \
    && [ "$(leftovers "$r/data")" = 0 ] && [ ! -e "$r/data/install.lock" ] \
    && [ -e "$r/data/notes" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' data='$(ls -A "$r/data")' inst='$(ls -A "$r/inst")' err='$err'"; fi

# The plugin data directory may be the install directory itself, also under
# another spelling (a trailing slash, a link to it): then its lock is the one
# in use and stays.
for how in 'the same path' 'a trailing slash' 'a link'; do
    t="plugin data directory is the install directory ($how)"
    r=$(make_repo)
    make_asset "$r"
    case "$how" in
        'the same path' ) data="$r/inst" ;;
        'a trailing slash' ) data="$r/inst/" ;;
        * ) mkdir -p "$r/inst"; ln -s "$r/inst" "$r/link"; data="$r/link" ;;
    esac
    run "$r" CLAUDE_PLUGIN_DATA="$data" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
    if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -f "$r/inst/install.lock" ] \
        && lock_free "$r/inst"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' err='$err'"; fi
done

# (xvi) A release file that matches its checksum but is not the program of
# the wanted version (an older one under the new name, or one that gives no
# version) is not installed and not run, and nothing is built in its place.
for kind in older 'no version'; do
    t="asset reports $kind"
    r=$(make_repo)
    make_cargo_stub "$r" ''
    mkdir -p "$r/rel/v$version" "$r/inst/bin"
    if [ "$kind" = older ]; then have=9.8.6; else have=''; fi
    make_fake "$r/rel/v$version/$asset" "$r/ran" '' "$have"
    printf '%s  %s\n' "$(shasum -a 256 "$r/rel/v$version/$asset" | awk '{print $1}')" "$asset" \
        > "$r/release/SHA256SUMS"
    make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' 9.8.5
    ino=$(inode "$r/inst/bin/hxprobe")
    run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/ran" ] && [ ! -e "$r/old-ran" ] \
        && [ ! -e "$r/builds" ] && [ "$(inode "$r/inst/bin/hxprobe")" = "$ino" ] \
        && [[ "$err" == *"is not hxprobe $version"* ]] && [[ "$err" != *'installed hxprobe'* ]] \
        && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' err='$err'"; fi
done

# (xvii) Where the installed program came from. A program built here (the
# record says so, or there is none) is replaced by the release program of
# the same version once the checksum file lists one; the release program is
# then run without another download.
for record in build none; do
    t="built program replaced by the release (record: $record)"
    r=$(make_repo)
    make_asset "$r"
    make_curl_probe "$r"
    make_cargo_stub "$r" ''
    mkdir -p "$r/inst/bin"
    make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' "$version"
    if [ "$record" = build ]; then echo build > "$r/inst/bin/.origin"; fi
    ino=$(inode "$r/inst/bin/hxprobe")
    want=$(awk '{ print $1 }' "$r/release/SHA256SUMS")
    run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    rc1=$rc; out1=$out; err1=$err
    ino1=$(inode "$r/inst/bin/hxprobe")
    rm -f "$r/ran"
    run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    if [ "$rc1" = 0 ] && [ "$out1" = 'FAKE mcp' ] && [ ! -e "$r/old-ran" ] \
        && [ "$ino1" != "$ino" ] && cmp -s "$r/inst/bin/hxprobe" "$r/rel/v$version/$asset" \
        && [ "$(origin_of "$r/inst")" = "release $want" ] && [ "$(cat "$r/curl-lock")" = held ] \
        && [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -e "$r/ran" ] && [ -z "$err" ] \
        && [ "$(inode "$r/inst/bin/hxprobe")" = "$ino1" ] \
        && [ "$(wc -l < "$r/curl-args" | tr -d ' ')" = 1 ] && [ ! -e "$r/builds" ] \
        && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst"; then pass "$t"
    else fail "$t" "rc=$rc1/$rc out='$out1'/'$out' origin='$(origin_of "$r/inst")' err='$err1' / '$err'"; fi
done

# (xvii-b) A download that fails is no reason to build while a program that
# answers is installed: that one is run, with a log line, and stays as it is.
# This holds for a built program of the wanted version and for an older one.
for have in "$version" 9.8.6; do
    t="download fails, installed $have is run"
    r=$(make_repo)
    make_asset "$r"
    make_curl_fail "$r"
    make_cargo_stub "$r" ''
    mkdir -p "$r/inst/bin"
    make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' "$have"
    echo build > "$r/inst/bin/.origin"
    ino=$(inode "$r/inst/bin/hxprobe")
    run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" -- x
    if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp x' ] && [ -e "$r/old-ran" ] && [ ! -e "$r/ran" ] \
        && [ ! -e "$r/builds" ] && [ "$(inode "$r/inst/bin/hxprobe")" = "$ino" ] \
        && [ "$(origin_of "$r/inst")" = build ] && [ "$(cat "$r/old-ran")" = "$fds_closed" ] \
        && [[ "$err" == *"download of $asset failed"* ]] \
        && [[ "$err" == *"running the installed hxprobe $have"* ]] \
        && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' builds='$(cat "$r/builds" 2>/dev/null || true)' err='$err'"; fi
done

# (xvii-b2) After a failed download with a built program of the wanted
# version installed, the download is not tried on every start: a stamp
# (bin/.download-failed) holds it back for about 6 hours. A second start runs
# the installed program without curl; with a stamp of 7 hours ago the
# download is tried again, and an install removes the stamp.
t='failed download is not tried again at once'
r=$(make_repo)
make_asset "$r"
make_curl_fail "$r"
make_cargo_stub "$r" ''
mkdir -p "$r/inst/bin"
make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' "$version"
echo build > "$r/inst/bin/.origin"
stamp="$r/inst/bin/.download-failed"
curls() { { cat "$r/curl-args" 2>/dev/null || true; } | wc -l | tr -d ' '; }
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
n1=$(curls); s1=no; if [ -f "$stamp" ]; then s1=yes; fi
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
rc2=$rc; out2=$out; err2=$err; n2=$(curls)
touch -t "$(date -v-7H +%Y%m%d%H%M)" "$stamp"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
rc3=$rc; out3=$out; n3=$(curls)
# The stamp is new again; make it old and let the download succeed.
touch -t "$(date -v-7H +%Y%m%d%H%M)" "$stamp"
make_curl_probe "$r"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$n1" = 1 ] && [ "$s1" = yes ] \
    && [ "$rc2" = 0 ] && [ "$out2" = 'FAKE mcp' ] && [ -z "$err2" ] && [ "$n2" = 1 ] \
    && [ "$rc3" = 0 ] && [ "$out3" = 'FAKE mcp' ] && [ "$n3" = 2 ] \
    && [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -e "$r/ran" ] && [ ! -e "$stamp" ] \
    && [[ "$(origin_of "$r/inst")" == 'release '* ]] && [ ! -e "$r/builds" ] \
    && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst"; then pass "$t"
else fail "$t" "curls=$n1/$n2/$n3 stamp1=$s1 rc=$rc2/$rc3/$rc out='$out2'/'$out3'/'$out' err='$err2' / '$err'"; fi

# An older installed program is run after a failed download as well, but
# nothing is held back: the next start tries the download again.
t='failed download with an older program is tried again'
r=$(make_repo)
make_asset "$r"
make_curl_fail "$r"
mkdir -p "$r/inst/bin"
make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' 9.8.6
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(curls)" = 2 ] \
    && [ ! -e "$r/inst/bin/.download-failed" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' curls=$(curls) err='$err'"; fi

# (xvii-c) With nothing usable installed, a failed download falls back to a
# build; the record says that the program was built here.
t='download fails, nothing installed: build'
r=$(make_repo)
make_asset "$r"
make_curl_fail "$r"
make_cargo_stub "$r" ''
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'BUILT mcp' ] && [ "$(origin_of "$r/inst")" = build ] \
    && [[ "$err" == *"download of $asset failed"* ]] \
    && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (xvii-d) A checksum mismatch is never answered with a build. With nothing
# of the wanted version installed the launcher stops. With a built program of
# the wanted version that program is run, the file is deleted, the mismatch
# is logged and the download is held back like a failed one: the next start
# runs the program without a word.
for installed in nothing built; do
    t="checksum mismatch with cargo on PATH, $installed installed"
    r=$(make_repo)
    make_asset "$r"
    printf '%s  %s\n' "$(printf '0%.0s' $(seq 64))" "$asset" > "$r/release/SHA256SUMS"
    make_cargo_stub "$r" ''
    mkdir -p "$r/inst/bin"
    ino=none
    if [ "$installed" = built ]; then
        make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' "$version"
        echo build > "$r/inst/bin/.origin"
        ino=$(inode "$r/inst/bin/hxprobe")
    fi
    run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    ok=0
    if [ "$installed" = nothing ]; then
        if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/old-ran" ] \
            && [ ! -e "$r/inst/bin/.download-failed" ]; then ok=1; fi
    elif [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(cat "$r/old-ran")" = "$fds_closed" ] \
        && [ "$(origin_of "$r/inst")" = build ] && [ -f "$r/inst/bin/.download-failed" ] \
        && [[ "$err" == *"running the installed hxprobe $version"* ]]; then
        err1=$err
        run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
        if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -z "$err" ]; then ok=1; fi
        err=$err1
    fi
    if [ "$ok" = 1 ] && [ ! -e "$r/ran" ] \
        && [ ! -e "$r/builds" ] && [ "$(inode "$r/inst/bin/hxprobe")" = "$ino" ] \
        && [[ "$err" == *'checksum mismatch'* ]] && [[ "$err" != *'building hxprobe'* ]] \
        && [ "$(leftovers "$r/inst")" = 0 ] && lock_free "$r/inst"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' err='$err'"; fi
done

# (xvii-e) Without a checksum line for the version, a built program of that
# version is all there is: it is run without a download.
t='built program without a release is run'
r=$(make_repo)
make_curl_fail "$r"
make_cargo_stub "$r" ''
mkdir -p "$r/inst/bin"
make_fake "$r/inst/bin/hxprobe" "$r/old-ran" '' "$version"
echo build > "$r/inst/bin/.origin"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -z "$err" ] && [ ! -e "$r/curl-args" ] \
    && [ ! -e "$r/builds" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (xviii) The cargo build with a space in the install path (the shared
# directory of the tests has one, the fallback under HOME too): the build
# directory is passed to cargo in one piece and the program is installed and
# run from that path.
t='cargo build, space in the install path'
r=$(make_repo)
make_cargo_stub "$r" 'for a; do printf "%s\n" "$a"; done > "'"$r"'/cargo-argv"'
d=$(shared_of "$r")
rm -rf "$root/home"
run "$r" HXPROBE_INSTALL_DIR= PATH="$r/stub:$nocargo_path" -- 'a b'
target=$(awk 'prev == "--target-dir" { print; exit } { prev = $0 }' "$r/cargo-argv" 2>/dev/null || true)
if [ "$rc" = 0 ] && [ "$out" = 'BUILT mcp a b' ] && [ -x "$d/bin/hxprobe" ] \
    && [[ "$target" == "$d/target."* ]] && [ "$(origin_of "$d")" = build ] \
    && [ "$(leftovers "$d")" = 0 ] && lock_free "$d" && [ ! -e "$r/inst" ] \
    && [ ! -e "$root/home" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' target='$target' err='$err'"; fi
rm -rf "$root/home"

# (xix) Which shared directory is used. A chain that is not the user's own
# (or root's with a listed program, see (xxi)) is never used: the launcher falls back to
# $HOME/Library/Application Support/hxprobe, says why, and writes nothing
# through or into the shared path.
own_dir="$root/home/Library/Application Support/hxprobe"

# A listing of everything at and below $1 (not following links): name, mode,
# inode, modification time and size, or `none`.
tree_state() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        find "$1" -exec stat -f '%N %p %i %m %z' {} + 2>/dev/null || true
    else
        echo none
    fi
}

# Succeeds when the last run fell back from shared directory $1 of repo $r:
# the release program was installed into and run from the per-user
# directory, and stderr names the shared program and a reason with $2.
fell_back() {
    [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] \
        && [ "$(cat "$r/ran.path" 2>/dev/null || true)" = "$own_dir/bin/hxprobe" ] \
        && [[ "$err" == *"the shared program $1/bin/hxprobe was not used ("*"$2"*"); a device profile for that path does not cover this program"* ]]
}

# The longest note the server appends to an error (it cuts a longer one),
# read from its source.
max_note=$(sed -n 's/^const MAX_NOTE_CHARS: usize = \([0-9][0-9]*\);.*/\1/p' "$here/../hxprobe/src/mcp/proto.rs" 2>/dev/null || true)

# Succeeds when the note of the last run (taken from the launcher's log
# line) would reach the user whole on a real Mac: with the real shared
# directory in place of the one of the case ($1) it has at most $max_note
# characters, and it names no path inside the plugin ($r), whose length
# nobody bounds.
note_fits() {
    local n
    n=$(printf '%s\n' "$err" \
        | sed -n 's/^hxprobe plugin: \(the shared program .* does not cover this program\); using .*/\1/p' \
        | head -n 1)
    [ -n "$n" ] && [ -n "$max_note" ] || return 1
    n=${n//"$1"/$real_shared}
    n=${n//"$(dirname "$1")"/$(dirname "$real_shared")}
    case "$n" in
        *"$r"* ) return 1 ;;
    esac
    [ "${#n}" -le "$max_note" ]
}

t='note limit of the server found'
if [ -n "$max_note" ]; then pass "$t"; else fail "$t" 'no MAX_NOTE_CHARS in hxprobe/src/mcp/proto.rs'; fi

# Another uid than the one running the tests.
other_uid=4242
if [ "$(id -u)" = "$other_uid" ]; then other_uid=4243; fi

for kind in 'a link to a directory' 'a regular file' 'bin is a link' 'the program is a link' \
        'mode 775' 'mode 777' 'an access list' 'the parent is missing' \
        'the parent is writable by others' 'another owner'; do
    t="shared directory not used: $kind"
    r=$(make_repo)
    make_asset "$r"
    rm -rf "$root/home"
    s=$(shared_of "$r")
    mkdir -p "$r/elsewhere"
    phrase=''
    case "$kind" in
        'a link to a directory' ) ln -s "$r/elsewhere" "$s"; phrase="$s is a symbolic link" ;;
        'a regular file' ) printf 'x\n' > "$s"; phrase="$s is not a directory" ;;
        'bin is a link' ) mkdir -m 755 "$s"; ln -s "$r/elsewhere" "$s/bin"; phrase="$s/bin is a symbolic link" ;;
        'the program is a link' )
            mkdir -m 755 "$s" "$s/bin"
            make_fake "$r/elsewhere/hxprobe" "$r/link-ran" '' "$version"
            ln -s "$r/elsewhere/hxprobe" "$s/bin/hxprobe"
            phrase="$s/bin/hxprobe is a symbolic link" ;;
        'mode 775' | 'mode 777' )
            mkdir "$s"; chmod "${kind#mode }" "$s"; phrase="$s is writable by group or others" ;;
        'an access list' )
            mkdir -m 755 "$s"
            chmod +a 'group:everyone allow list' "$s"
            phrase="$s has an access control list" ;;
        'the parent is missing' ) s="$r/missing/hxprobe"; phrase="$r/missing does not exist" ;;
        'the parent is writable by others' )
            chmod 777 "$(dirname "$s")"; phrase="$(dirname "$s") is writable by others and not sticky" ;;
        'another owner' )
            # A chain of another account with the release program, whatever
            # its checksum: the ls stub shows another owner.
            mkdir -m 755 "$s" "$s/bin"
            cp "$r/rel/v$version/$asset" "$s/bin/hxprobe"
            echo "release $(awk '{ print $1 }' "$r/release/SHA256SUMS")" > "$s/bin/.origin"
            make_ls_owner "$r" "$other_uid"; phrase="$s is owned by another account (uid $other_uid)" ;;
    esac
    before=$(tree_state "$s"; tree_state "$r/elsewhere")
    run "$r" HXPROBE_INSTALL_DIR= HXPROBE_SHARED_DIR="$s" HXPROBE_RELEASE_BASE="file://$r/rel" \
        PATH="$r/stub:$nocargo_path" --
    after=$(tree_state "$s"; tree_state "$r/elsewhere")
    if fell_back "$s" "$phrase" && note_fits "$s" && [ "$after" = "$before" ] && [ ! -e "$r/link-ran" ] \
        && [ "$(origin_of "$own_dir")" != none ] && lock_free "$own_dir"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' ran='$(cat "$r/ran.path" 2>/dev/null || true)' changed=$([ "$after" = "$before" ] && echo no || echo yes) err='$err'"; fi
done
rm -rf "$root/home"

# A parent that everyone may write to but that is sticky, as /Users/Shared
# is (mode 1777), is safe: the shared directory is created and used.
t='shared directory in a sticky parent'
r=$(make_repo)
make_asset "$r"
s=$(shared_of "$r")
chmod 1777 "$(dirname "$s")"
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(cat "$r/ran.path")" = "$s/bin/hxprobe" ] \
    && [ ! -e "$root/home" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# The second look after mkdir: what is on disk decides, not what mkdir
# reported. A mkdir stub that makes the shared directory and then fails (as
# when another start made it in between) leads to the install there; one
# that puts a link at the shared path and fails leads to the fallback.
for kind in 'made it' 'left a link'; do
    t="mkdir fails and $kind"
    r=$(make_repo)
    make_asset "$r"
    rm -rf "$root/home"
    s=$(shared_of "$r")
    mkdir -p "$r/elsewhere" "$r/stub"
    if [ "$kind" = 'made it' ]; then act='/bin/mkdir "$@"'; else act="ln -s '$r/elsewhere' '$s'"; fi
    cat > "$r/stub/mkdir" <<EOF
#!/bin/sh
for a; do p=\$a; done
if [ "\$p" = '$s' ]; then $act; exit 1; fi
exec /bin/mkdir "\$@"
EOF
    chmod 755 "$r/stub/mkdir"
    run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    if [ "$kind" = 'made it' ]; then
        if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(cat "$r/ran.path")" = "$s/bin/hxprobe" ] \
            && [[ "$err" != *'was not used'* ]] && [ ! -e "$root/home" ]; then pass "$t"
        else fail "$t" "rc=$rc out='$out' err='$err'"; fi
    else
        if fell_back "$s" "$s is a symbolic link" && [ "$(tree_state "$r/elsewhere" | wc -l | tr -d ' ')" = 1 ]; then pass "$t"
        else fail "$t" "rc=$rc out='$out' err='$err'"; fi
    fi
done
rm -rf "$root/home"

# Many launchers at once in shared mode: one install into the shared
# directory (each start makes or finds the directory), every start answers,
# the lock is free afterwards and nothing is left over.
t='concurrent starts, shared directory'
r=$(make_repo)
make_cargo_stub "$r" 'sleep 0.3'
s=$(shared_of "$r")
rm -rf "$root/home"
run_many "$r" "$many" "$r/stub:$nocargo_path" ''
builds=$(wc -l < "$r/builds" | tr -d ' ')
installs=$(cat "$r"/err.* | grep -c 'installed hxprobe' || true)
if [ "$bad" = 0 ] && [ "$builds" = 1 ] && [ "$installs" = 1 ] && [ -x "$s/bin/hxprobe" ] \
    && [ "$(leftovers "$s")" = 0 ] && lock_free "$s" && [ ! -e "$r/inst" ] \
    && ! grep -q 'was not used' "$r"/err.* && [ ! -e "$root/home" ]; then pass "$t"
else fail "$t" "bad=$bad builds=$builds installs=$installs leftovers=$(leftovers "$s") err='$(cat "$r"/err.*)'"; fi
rm -rf "$root/home"

# The note for the server. After a fallback the program gets the reason in
# HXPROBE_INSTALL_NOTE, in place of an inherited value, and the fallback
# directory has mode 700. Without a fallback, with HXPROBE_BIN and with an
# explicit HXPROBE_INSTALL_DIR an inherited value is removed.
for how in fallback 'shared directory used' HXPROBE_BIN 'HXPROBE_INSTALL_DIR given'; do
    t="install note: $how"
    r=$(make_repo)
    make_asset "$r"
    rm -rf "$root/home"
    s=$(shared_of "$r")
    note='(unset)'
    case "$how" in
        fallback )
            printf 'x\n' > "$s"
            note="the shared program $s/bin/hxprobe was not used ($s is not a directory); a device profile for that path does not cover this program"
            run "$r" HXPROBE_INSTALL_DIR= HXPROBE_INSTALL_NOTE=stale HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" -- ;;
        'shared directory used' )
            run "$r" HXPROBE_INSTALL_DIR= HXPROBE_INSTALL_NOTE=stale HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" -- ;;
        HXPROBE_BIN )
            make_fake "$r/fake" "$r/ran" '' "$version"
            run "$r" HXPROBE_BIN="$r/fake" HXPROBE_INSTALL_NOTE=stale PATH="$nocargo_path" -- ;;
        * )
            run "$r" HXPROBE_INSTALL_NOTE=stale HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" -- ;;
    esac
    got=$(cat "$r/ran.note" 2>/dev/null || echo 'no marker')
    ok=1
    if [ "$how" = fallback ]; then
        fell_back "$s" "$s is not a directory" && [ "$(mode_of "$own_dir")" = 700 ] \
            && [ "$(mode_of "$own_dir/bin")" = 700 ] && [ "$(mode_of "$own_dir/bin/hxprobe")" = 700 ] || ok=0
    fi
    if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$got" = "$note" ] && [ "$ok" = 1 ]; then pass "$t"
    else fail "$t" "rc=$rc out='$out' note='$got' err='$err'"; fi
done
rm -rf "$root/home"

# Migration from the per-user directory: the first install into the shared
# directory with an earlier program at $HOME/Library/Application
# Support/hxprobe/bin/hxprobe logs once that the plugin no longer uses it, and
# leaves its files as they are; a later start does not log it again.
t='earlier install is named once'
r=$(make_repo)
make_asset "$r"
rm -rf "$root/home"
s=$(shared_of "$r")
mkdir -p "$own_dir/bin"
make_fake "$own_dir/bin/hxprobe" "$r/old-ran" '' 0.2.0
echo "release $(printf '0%.0s' $(seq 64))" > "$own_dir/bin/.origin"
: > "$own_dir/bin/.download-failed"
: > "$own_dir/install.lock"
before=$(tree_state "$own_dir")
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
rc1=$rc; out1=$out; err1=$err
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" --
line="the earlier install $own_dir/bin/hxprobe is no longer used by the plugin"
if [ "$rc1" = 0 ] && [ "$out1" = 'FAKE mcp' ] && [ "$(cat "$r/ran.path")" = "$s/bin/hxprobe" ] \
    && [ "$(printf '%s\n' "$err1" | grep -cF "$line")" = 1 ] \
    && [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [[ "$err" != *"$line"* ]] \
    && [ "$(tree_state "$own_dir")" = "$before" ] && [ ! -e "$r/old-ran" ]; then pass "$t"
else fail "$t" "rc=$rc1/$rc out='$out1'/'$out' err='$err1' / '$err'"; fi
rm -rf "$root/home"

# (xx) The SHA-256 of the installed program is checked at every start. In
# the shared directory (the user's own) and, for the changed file, in an
# explicit install directory too.

# Puts file $2 as the program into install directory $1 (mode 755) with the
# origin record $3.
put_program() {
    mkdir -p "$1/bin"
    chmod 755 "$1" "$1/bin"
    cp "$2" "$1/bin/hxprobe"
    chmod 755 "$1/bin/hxprobe"
    printf '%s\n' "$3" > "$1/bin/.origin"
}

# The SHA-256 of file $1.
sum_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }

# A program whose sum is listed for the wanted version runs without being
# asked for its version and without a download.
t='checksum: listed for the wanted version'
r=$(make_repo)
make_asset "$r"
make_curl_probe "$r"
rm -rf "$root/home"
s=$(shared_of "$r")
put_program "$s" "$r/rel/v$version/$asset" "release $(sum_of "$r/rel/v$version/$asset")"
ino=$(inode "$s/bin/hxprobe")
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -z "$err" ] && [ "$(cat "$r/ran.path")" = "$s/bin/hxprobe" ] \
    && [ ! -e "$r/ran.asked" ] && [ ! -e "$r/curl-args" ] && [ "$(inode "$s/bin/hxprobe")" = "$ino" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' asked=$([ -e "$r/ran.asked" ] && echo yes || echo no) err='$err'"; fi

# One whose sum is listed for an older version is replaced, without being
# asked either.
t='checksum: listed for an older version'
r=$(make_repo)
make_asset "$r"
make_curl_probe "$r"
rm -rf "$root/home"
s=$(shared_of "$r")
make_fake "$r/older" "$r/old-ran" '' 9.8.6
printf '%s  %s\n' "$(sum_of "$r/older")" "hxprobe-9.8.6-macos-$arch" >> "$r/release/SHA256SUMS"
put_program "$s" "$r/older" "release $(sum_of "$r/older")"
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(cat "$r/ran.path")" = "$s/bin/hxprobe" ] \
    && cmp -s "$s/bin/hxprobe" "$r/rel/v$version/$asset" && [ ! -e "$r/old-ran" ] \
    && [ ! -e "$r/old-ran.asked" ] && [ -e "$r/curl-args" ] \
    && [ "$(origin_of "$s")" = "release $(sum_of "$r/rel/v$version/$asset")" ] && lock_free "$s"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# A release program that changed after the install (its sum is no longer
# the one recorded in bin/.origin) is not run, not even asked for its
# version: the release program is downloaded again and run. When that
# download fails the launcher stops and names the checksum. The record
# decides, whatever version it was written for: after a plugin upgrade the
# record of the older release program counts the same. Without a release
# program for the wanted version the changed file is replaced by a build.
for how in 'shared directory' 'install dir given' 'download fails' 'record of an older version' \
        'no release listed'; do
    t="checksum: changed release file, $how"
    r=$(make_repo)
    make_asset "$r"
    rm -rf "$root/home"
    if [ "$how" = 'download fails' ]; then make_curl_fail "$r"; else make_curl_probe "$r"; fi
    make_cargo_stub "$r" ''
    if [ "$how" = 'install dir given' ]; then d="$r/inst"; inst_set="HXPROBE_INSTALL_DIR=$r/inst"
    else d=$(shared_of "$r"); inst_set='HXPROBE_INSTALL_DIR='; fi
    make_fake "$r/changed" "$r/changed-ran" '' "$version"
    record="release $(sum_of "$r/rel/v$version/$asset")"
    case "$how" in
        'record of an older version' | 'no release listed' )
            make_fake "$r/older" "$r/old-ran" '' 9.8.6
            record="release $(sum_of "$r/older")" ;;
    esac
    if [ "$how" = 'no release listed' ]; then printf '# comment\n\n' > "$r/release/SHA256SUMS"; fi
    put_program "$d" "$r/changed" "$record"
    run "$r" "$inst_set" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    ok=0
    if [ "$how" = 'download fails' ]; then
        if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/ran" ] && [[ "$err" == *checksum* ]] \
            && [ ! -e "$r/builds" ] && cmp -s "$d/bin/hxprobe" "$r/changed"; then ok=1; fi
    elif [ "$how" = 'no release listed' ]; then
        if [ "$rc" = 0 ] && [ "$out" = 'BUILT mcp' ] && [ "$(origin_of "$d")" = build ] \
            && [ ! -e "$r/curl-args" ] && [[ "$err" == *checksum* ]]; then ok=1; fi
    elif [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(cat "$r/ran.path" 2>/dev/null || true)" = "$d/bin/hxprobe" ] \
        && cmp -s "$d/bin/hxprobe" "$r/rel/v$version/$asset" && [[ "$err" == *checksum* ]] \
        && [ ! -e "$r/builds" ]; then ok=1; fi
    if [ "$ok" = 1 ] && [ ! -e "$r/changed-ran" ] && [ ! -e "$r/changed-ran.asked" ] \
        && [ "$(leftovers "$d")" = 0 ] && lock_free "$d"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' changed-run=$([ -e "$r/changed-ran" ] || [ -e "$r/changed-ran.asked" ] && echo yes || echo no) err='$err'"; fi
done

# Without a release program for the wanted version and without cargo, the
# last message says why the installed program was not run.
t='checksum: changed release file, no release listed, no cargo'
r=$(make_repo)
rm -rf "$root/home"
d=$(shared_of "$r")
make_fake "$r/changed" "$r/changed-ran" '' "$version"
make_fake "$r/older" "$r/old-ran" '' 9.8.6
put_program "$d" "$r/changed" "release $(sum_of "$r/older")"
run "$r" HXPROBE_INSTALL_DIR= PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *'https://rustup.rs'* ]] \
    && [[ "${err##*$'\n'}" == *'the installed program was not run because it changed since its install'* ]] \
    && [ ! -e "$r/changed-ran" ] && [ ! -e "$r/changed-ran.asked" ] \
    && cmp -s "$d/bin/hxprobe" "$r/changed" && lock_free "$d"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# A signal that arrives between the write of the origin record and the move
# of the program (here: while the mode of the record is set) ends the
# launcher only after the move: the record and the program agree, and the
# server is not started.
t='signal between the origin record and the move'
r=$(make_repo)
make_asset "$r"
make_curl_probe "$r"
rm -rf "$root/home"
s=$(shared_of "$r")
make_fake "$r/older" "$r/old-ran" '' 9.8.6
put_program "$s" "$r/older" "release $(sum_of "$r/older")"
cat > "$r/stub/chmod" <<EOF
#!/bin/sh
for a; do p=\$a; done
if [ "\$p" = '$s/bin/.origin' ] && [ ! -e '$r/signalled' ]; then
    : > '$r/signalled'
    kill -TERM "\$PPID"
fi
exec /bin/chmod "\$@"
EOF
chmod 755 "$r/stub/chmod"
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 143 ] && [ -z "$out" ] && [ -e "$r/signalled" ] && [ ! -e "$r/ran" ] && [ ! -e "$r/old-ran" ] \
    && cmp -s "$s/bin/hxprobe" "$r/rel/v$version/$asset" \
    && [ "$(origin_of "$s")" = "release $(sum_of "$s/bin/hxprobe")" ] \
    && [ "$(leftovers "$s")" = 0 ] && lock_free "$s"; then pass "$t"
else fail "$t" "rc=$rc out='$out' signalled=$([ -e "$r/signalled" ] && echo yes || echo no) origin='$(origin_of "$s")' program=$(sum_of "$s/bin/hxprobe") err='$err'"; fi

# Today's rules for a program that is not listed: one built here runs by its
# --version answer; a newer one is kept and logged.
for kind in built newer; do
    t="checksum: not listed, $kind"
    r=$(make_repo)
    make_curl_probe "$r"
    rm -rf "$root/home"
    s=$(shared_of "$r")
    if [ "$kind" = built ]; then
        make_fake "$r/prog" "$r/prog-ran" '' "$version"
        put_program "$s" "$r/prog" build
    else
        make_asset "$r"
        make_fake "$r/prog" "$r/prog-ran" '' 9.10.0
        put_program "$s" "$r/prog" "release $(sum_of "$r/prog")"
    fi
    run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
    if [ "$kind" = built ]; then note_ok=$([ -z "$err" ] && echo 1 || echo 0)
    else note_ok=$([[ "$err" == *'9.10.0'*'newer'* ]] && echo 1 || echo 0); fi
    if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -e "$r/prog-ran" ] && [ -e "$r/prog-ran.asked" ] \
        && [ ! -e "$r/curl-args" ] && cmp -s "$s/bin/hxprobe" "$r/prog" && [ "$note_ok" = 1 ]; then pass "$t"
    else fail "$t" "rc=$rc out='$out' err='$err'"; fi
done

# Without shasum on PATH the program counts as not listed: an own one runs by
# its --version answer. PATH holds every program of /usr/bin but shasum.
t='checksum: no shasum'
r=$(make_repo)
make_asset "$r"
make_curl_probe "$r"
rm -rf "$root/home"
s=$(shared_of "$r")
mkdir -p "$r/tools"
for f in /usr/bin/*; do
    case "${f##*/}" in shasum ) ;; * ) ln -s "$f" "$r/tools/${f##*/}" ;; esac
done
put_program "$s" "$r/rel/v$version/$asset" "release $(sum_of "$r/rel/v$version/$asset")"
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$r/tools:/bin" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ "$(cat "$r/ran.path")" = "$s/bin/hxprobe" ] \
    && [ -e "$r/ran.asked" ] && [ ! -e "$r/curl-args" ] && [ -z "$err" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi
rm -rf "$root/home"

# (xxi) A shared directory owned by root (the ls stub shows uid 0) is used
# read-only, and only for a program whose SHA-256 is listed: nothing is
# written there, no lock is taken, nothing is downloaded and no mode is set.

# A chmod stub in $1/stub that records its arguments in $1/chmod-args and
# runs the real chmod.
make_chmod_probe() {
    mkdir -p "$1/stub"
    printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "%s/chmod-args"\nexec /bin/chmod "$@"\n' "$1" > "$1/stub/chmod"
    chmod 755 "$1/stub/chmod"
}

for kind in wanted older; do
    t="root-owned shared program listed, $kind version"
    r=$(make_repo)
    make_asset "$r"
    make_curl_probe "$r"
    make_chmod_probe "$r"
    make_ls_owner "$r" 0
    rm -rf "$root/home"
    s=$(shared_of "$r")
    if [ "$kind" = wanted ]; then
        prog="$r/rel/v$version/$asset"; marker="$r/ran"
    else
        make_fake "$r/older" "$r/old-ran" '' 9.8.6
        printf '%s  %s\n' "$(sum_of "$r/older")" "hxprobe-9.8.6-macos-$arch" >> "$r/release/SHA256SUMS"
        prog="$r/older"; marker="$r/old-ran"
    fi
    put_program "$s" "$prog" 'root record'
    before=$(tree_state "$s")
    run "$r" HXPROBE_INSTALL_DIR= HXPROBE_INSTALL_NOTE=stale HXPROBE_RELEASE_BASE="file://$r/rel" \
        PATH="$r/stub:$nocargo_path" -- a
    if [ "$kind" = wanted ]; then log_ok=$([ -z "$err" ] && echo 1 || echo 0)
    else log_ok=$([[ "$err" == *'9.8.6'*'administrator'* ]] && echo 1 || echo 0); fi
    if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp a' ] && [ "$(cat "$marker.path" 2>/dev/null || true)" = "$s/bin/hxprobe" ] \
        && [ "$(cat "$marker.note" 2>/dev/null || true)" = '(unset)' ] && [ "$log_ok" = 1 ] \
        && [ ! -e "$marker.asked" ] && [ ! -e "$s/install.lock" ] && [ "$(origin_of "$s")" = 'root record' ] \
        && [ ! -e "$r/chmod-args" ] && [ ! -e "$r/curl-args" ] && [ "$(tree_state "$s")" = "$before" ] \
        && [ ! -e "$root/home" ] && [ ! -e "$r/inst" ]; then pass "$t"
    else fail "$t" "rc=$rc out='$out' asked=$([ -e "$marker.asked" ] && echo yes || echo no) chmod='$(cat "$r/chmod-args" 2>/dev/null || true)' err='$err'"; fi
done

# A root chain that cannot be used: the program in it is never executed (no
# marker of any kind), and the per-user program is installed and run with
# the note. Without shasum nothing can be verified, the download included,
# so the per-user program is built. The note of the program that is not
# listed is the longest one the launcher writes; it names the checksum file
# as release/SHA256SUMS, not by its full path.
for kind in 'not listed' 'no program' 'mode 775' 'no shasum'; do
    t="root-owned shared directory not used: $kind"
    r=$(make_repo)
    make_asset "$r"
    make_cargo_stub "$r" ''
    make_ls_owner "$r" 0
    rm -rf "$root/home"
    s=$(shared_of "$r")
    path="$r/stub:$nocargo_path"
    if [ "$kind" = 'not listed' ]; then
        make_fake "$r/shared-prog" "$r/shared-ran" '' "$version"
    else
        make_fake "$r/shared-prog" "$r/shared-ran" '' 9.8.6
        printf '%s  %s\n' "$(sum_of "$r/shared-prog")" "hxprobe-9.8.6-macos-$arch" >> "$r/release/SHA256SUMS"
    fi
    put_program "$s" "$r/shared-prog" 'root record'
    case "$kind" in
        'not listed' ) phrase="$s/bin/hxprobe is owned by root and its SHA-256 is not listed for this machine in release/SHA256SUMS" ;;
        'no program' ) rm -f "$s/bin/hxprobe"; phrase="$s is owned by root and $s/bin/hxprobe does not exist" ;;
        'mode 775' ) chmod 775 "$s"; phrase="$s is writable by group or others" ;;
        'no shasum' )
            mkdir -p "$r/tools"
            for f in /usr/bin/*; do
                case "${f##*/}" in shasum ) ;; * ) ln -s "$f" "$r/tools/${f##*/}" ;; esac
            done
            path="$r/stub:$r/tools:/bin"
            phrase="$s/bin/hxprobe is owned by root and shasum is not available to check its SHA-256" ;;
    esac
    before=$(tree_state "$s")
    run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$path" --
    note="the shared program $s/bin/hxprobe was not used ($phrase"
    if [ "$kind" = 'no shasum' ]; then
        ok=0
        if [ "$rc" = 0 ] && [ "$out" = 'BUILT mcp' ] && [ "$(origin_of "$own_dir")" = build ] \
            && [[ "$err" == *"$note"*'); a device profile for that path does not cover this program'* ]]; then ok=1; fi
    else
        ok=0; fell_back "$s" "$phrase" && ok=1
        got=$(cat "$r/ran.note" 2>/dev/null || true)
        [[ "$got" == "$note"*'); a device profile for that path does not cover this program' ]] || ok=0
    fi
    if [ "$ok" = 1 ] && note_fits "$s" && [ ! -e "$r/shared-ran" ] && [ ! -e "$r/shared-ran.asked" ] \
        && [ "$(tree_state "$s")" = "$before" ] && [ ! -e "$s/install.lock" ] \
        && lock_free "$own_dir"; then pass "$t"
    else fail "$t" "rc=$rc out='$out' note='$(cat "$r/ran.note" 2>/dev/null || true)' err='$err'"; fi
done
rm -rf "$root/home"

# (last) No case reached the real shared directory: it is as it was before
# the run (absent, or the same inode and times).
t='real shared directory untouched'
if [ "$(real_state)" = "$real_before" ]; then pass "$t"
else fail "$t" "$real_shared was '$real_before' before the run and is '$(real_state)' now"; fi

if [ "$failures" -ne 0 ]; then
    printf '%d case(s) failed\n' "$failures"
    exit 1
fi
printf 'all cases passed\n'
