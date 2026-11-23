#!/bin/bash
# Tests for scripts/run-mcp.sh. No network: downloads come from file:// URLs,
# and the program is a small shell script that prints its arguments.
# Each case runs against a temporary copy of the repository layout, so the
# real release/SHA256SUMS is never touched, and with HOME, the install
# directory (HXPROBE_INSTALL_DIR) and the plugin data directory
# (CLAUDE_PLUGIN_DATA) in a temporary directory.

set -euo pipefail

here=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
launcher="$here/run-mcp.sh"
version=9.8.7

root=$(mktemp -d "${TMPDIR:-/tmp}/test-run-mcp.XXXXXX")
trap 'rm -rf "$root"' EXIT

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
# with a usage message and status 2, as 0.1.0 does). Otherwise it prints its
# arguments to stdout and leaves a marker file that holds the state of fd 3
# and fd 9, and the file `<marker>.path` with the path it was run as. $3 is
# optional shell code to run last.
make_fake() {
    # $1 = path, $2 = marker file, $3 = shell code, $4 = version
    cat > "$1" <<EOF
#!/bin/sh
if [ "\$1" = --version ]; then
    if [ -n "${4:-}" ]; then echo "hxprobe ${4:-}"; exit 0; fi
    echo "usage: hxprobe <command>" >&2
    exit 2
fi
echo "FAKE \$*"
$fd_probe
echo "fd3=\$f3 fd9=\$f9" > "$2"
printf '%s\n' "\$0" > "$2.path"
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
    mkdir -p "$r/scripts" "$r/hxprobe" "$r/release"
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
# install directory is $1/inst and the plugin data directory $1/data unless
# the settings say otherwise (an empty value selects the default).
run() {
    local r=$1
    shift
    local envs=''
    while [ "$1" != -- ]; do envs="$envs $1"; shift; done
    shift
    rc=0
    # shellcheck disable=SC2086
    env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS -u CLAUDE_PLUGIN_DATA \
        -u HXPROBE_INSTALL_DIR \
        HOME="$root/home" HXPROBE_INSTALL_DIR="$r/inst" CLAUDE_PLUGIN_DATA="$r/data" $envs \
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

# The pid of a process that has ended.
dead_pid() {
    sh -c 'exit 0' & local p=$!
    wait "$p"
    printf '%s\n' "$p"
}

# Succeeds when the process with pid $1 is gone, within about 1 s.
gone() {
    local i=0
    while kill -0 "$1" 2>/dev/null; do
        [ "$i" -lt 10 ] || return 1
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
# Sets bad to the number that failed or printed something else than the
# server output.
run_many() {
    local r=$1 n=$2 path=$3 i pids='' p
    bad=0
    i=0
    while [ "$i" -lt "$n" ]; do
        env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS \
            HOME="$root/home" HXPROBE_INSTALL_DIR="$r/inst" CLAUDE_PLUGIN_DATA="$r/data" \
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

# (ii) An installed program that reports the wanted version is run from the
# stable path without a download, without arguments too (bash 3.2 with set -u
# and an empty "$@"). Legacy programs stay: nothing was installed.
t='installed program of the version'
r=$(make_repo)
make_asset "$r"
make_curl_probe "$r"
mkdir -p "$r/inst/bin" "$r/data/bin"
make_fake "$r/inst/bin/hxprobe" "$r/inst-ran" '' "$version"
make_fake "$r/data/bin/hxprobe-0.1.0" "$r/old-ran"
run "$r" HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -z "$err" ] \
    && [ "$(cat "$r/inst-ran")" = "$fds_closed" ] \
    && [ "$(cat "$r/inst-ran.path")" = "$r/inst/bin/hxprobe" ] \
    && [ ! -e "$r/curl-args" ] && [ -e "$r/data/bin/hxprobe-0.1.0" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err' curl='$(cat "$r/curl-args" 2>/dev/null || true)'"; fi

# (ii-b) Default install directory under HOME: the program is installed as
# $HOME/Library/Application Support/hxprobe/bin/hxprobe, the directories
# with mode 0700, and run from there.
t='default install dir'
r=$(make_repo)
make_asset "$r"
d="$root/home/Library/Application Support/hxprobe"
run "$r" HXPROBE_INSTALL_DIR= HXPROBE_RELEASE_BASE="file://$r/rel" PATH="$nocargo_path" -- a b
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp a b' ] && [ "$(mode_of "$d")" = 700 ] \
    && [ "$(mode_of "$d/bin")" = 700 ] && [ "$(mode_of "$d/bin/hxprobe")" = 700 ] \
    && [ "$(cat "$r/ran.path")" = "$d/bin/hxprobe" ] && [ ! -e "$r/inst" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' modes=$(mode_of "$d")/$(mode_of "$d/bin") err='$err'"; fi
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
    && [[ "$err" == *'checksum of '"$asset"' verified'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' mode=$(mode_of "$b") left=$left err='$err'"; fi

# (iii-b) A non-file base URL is limited to https: http:// is refused by curl
# before any connection, so the launcher falls through to the final error.
t='download refuses http'
r=$(make_repo)
printf '%s  %s\n' "$sum" "$asset" > "$r/release/SHA256SUMS"
run "$r" HXPROBE_RELEASE_BASE="http://127.0.0.1:9" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *"download of $asset failed"* ]] \
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
        && [ "$(find "$r/inst/bin" -mindepth 1 | wc -l | tr -d ' ')" = 1 ] \
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
    && lock_free "$r/inst"; then pass "$t"
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
            mkdir -p "$r/inst/bin" "$r/inst/target/release" "$r/inst/target.$(dead_pid)/release"
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
make_fake "$r/rel/v$version/$asset" "$r/ran" 'sleep 2'
printf '%s  %s\n' "$(shasum -a 256 "$r/rel/v$version/$asset" | awk '{print $1}')" "$asset" > "$r/release/SHA256SUMS"
env -u HXPROBE_BIN -u HXPROBE_SUMS HOME="$root/home" HXPROBE_INSTALL_DIR="$r/inst" \
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

# (ix) TERM during a build or a download ends the launcher within about 2 s
# with a non-zero status; the lock is free and nothing is left behind. The
# command and what it started are gone as well: each stub starts a second
# process, which during a build keeps writing into the build directory. In
# the third variant that process ignores TERM, so the launcher has to kill it
# (after 2 s).
for what in build download 'build, TERM ignored'; do
    t="TERM during $what"
    r=$(make_repo)
    mkdir -p "$r/stub"
    limit=2
    ignore=''
    if [ "$what" = 'build, TERM ignored' ]; then limit=4; ignore="trap '' TERM"; fi
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
    env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS \
        HOME="$root/home" HXPROBE_INSTALL_DIR="$r/inst" CLAUDE_PLUGIN_DATA="$r/data" \
        PATH="$r/stub:$nocargo_path" \
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
env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS \
    HOME="$root/home" HXPROBE_INSTALL_DIR="$r/inst" CLAUDE_PLUGIN_DATA="$r/data" \
    PATH="$r/stub:$nocargo_path" \
    bash "$r/scripts/run-mcp.sh" > "$r/out" 2> "$r/err" &
pid=$!
# Two lockf runs: the attempt that fails at once and the one that waits.
i=0
while [ "$(cat "$r/lockf.pids" 2>/dev/null | wc -l)" -lt 2 ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
sleep 0.2
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
if [ "$(printf '%s\n' "$waiters" | wc -l | tr -d ' ')" = 2 ] && [ "$rc" = 143 ] && [ "$took" -le 2 ] \
    && [ ! -s "$r/out" ] && [ -z "$alive" ] && [[ "$(cat "$r/err")" == *'waiting'* ]] \
    && kill -0 "$holder" 2>/dev/null; then pass "$t"
else fail "$t" "rc=$rc took=${took}s waiters='$waiters' alive='$alive' err='$(cat "$r/err")'"; fi
for p in $waiters; do kill -KILL "$p" 2>/dev/null || true; done
kill -KILL "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true

if [ "$failures" -ne 0 ]; then
    printf '%d case(s) failed\n' "$failures"
    exit 1
fi
printf 'all cases passed\n'
