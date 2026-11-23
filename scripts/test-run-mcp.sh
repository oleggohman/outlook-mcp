#!/bin/bash
# Tests for scripts/run-mcp.sh. No network: downloads come from file:// URLs,
# and the program is a small shell script that prints its arguments.
# Each case runs against a temporary copy of the repository layout, so the
# real release/SHA256SUMS is never touched.

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

# Fake program: prints its arguments to stdout and leaves a marker file.
make_fake() {
    # $1 = path, $2 = marker file
    cat > "$1" <<EOF
#!/bin/sh
echo "FAKE \$*"
: > "$2"
EOF
    chmod 755 "$1"
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
# followed by `--` and launcher arguments. Sets out, err, rc.
run() {
    local r=$1
    shift
    local envs=''
    while [ "$1" != -- ]; do envs="$envs $1"; shift; done
    shift
    rc=0
    # shellcheck disable=SC2086
    env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS -u CLAUDE_PLUGIN_DATA \
        HOME="$root/home" $envs \
        bash "$r/scripts/run-mcp.sh" "$@" > "$root/out" 2> "$root/err" || rc=$?
    out=$(cat "$root/out")
    err=$(cat "$root/err")
}

# Succeeds when nobody holds the install lock of data directory $1.
lock_free() {
    [ ! -e "$1/install.lock" ] || lockf -k -s -t 0 "$1/install.lock" true
}

# Counts leftovers of an install in data directory $1: temp files and build
# directories.
leftovers() {
    find "$1" -mindepth 1 \( -name '.download.*' -o -name '.build.*' -o -name 'target' -o -name 'target.*' \) | wc -l | tr -d ' '
}

# The pid of a process that has ended.
dead_pid() {
    sh -c 'exit 0' & local p=$!
    wait "$p"
    printf '%s\n' "$p"
}

# A cargo stub in $1/stub: appends a line to $1/builds, runs the shell code $2
# and leaves a program where cargo would.
make_cargo_stub() {
    mkdir -p "$1/stub"
    cat > "$1/stub/cargo" <<EOF
#!/bin/sh
echo "cargo noise on stdout"
echo start >> "$1/builds"
$2
# Locate --target-dir and leave a program where cargo would.
while [ \$# -gt 0 ]; do
    if [ "\$1" = --target-dir ]; then t=\$2; fi
    shift
done
mkdir -p "\$t/release"
printf '#!/bin/sh\necho "BUILT \$*"\n' > "\$t/release/hxprobe"
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
            HOME="$root/home" CLAUDE_PLUGIN_DATA="$r/data" PATH="$path" \
            bash "$r/scripts/run-mcp.sh" > "$r/out.$i" 2> "$r/err.$i" &
        pids="$pids $!"
        i=$((i + 1))
    done
    i=0
    for p in $pids; do
        if ! wait "$p" || [ "$(cat "$r/out.$i")" != 'BUILT mcp' ]; then bad=$((bad + 1)); fi
        i=$((i + 1))
    done
}

# (i) HXPROBE_BIN runs the given program with `mcp` and the arguments.
t='HXPROBE_BIN'
r=$(make_repo)
make_fake "$r/fake" "$r/ran"
run "$r" HXPROBE_BIN="$r/fake" CLAUDE_PLUGIN_DATA="$r/data" -- --store /x
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp --store /x' ] && [ -z "$err" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (i-b) HXPROBE_BIN that is not executable is an error, not a fallback.
t='HXPROBE_BIN not executable'
run "$r" HXPROBE_BIN="$r/missing" CLAUDE_PLUGIN_DATA="$r/data" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == 'hxprobe plugin: HXPROBE_BIN is set'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (ii) An installed program for the version is run directly, without
# arguments too (bash 3.2 with set -u and an empty "$@").
t='cached program'
r=$(make_repo)
mkdir -p "$r/data/bin"
make_fake "$r/data/bin/hxprobe-$version" "$r/ran"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$nocargo_path" --
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [ -z "$err" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (ii-b) Default data directory under HOME, created with mode 0700.
t='default data dir'
r=$(make_repo)
d="$root/home/Library/Application Support/hxprobe-plugin"
mkdir -p "$d/bin"
make_fake "$d/bin/hxprobe-$version" "$r/ran"
run "$r" PATH="$nocargo_path" -- a b
mode=$(stat -f %Lp "$d/bin" 2>/dev/null || echo none)
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp a b' ] && [ "$mode" = 700 ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' mode=$mode err='$err'"; fi
rm -rf "$root/home"

# (iii) Download with a matching checksum: installed with mode 0700, other
# versions removed, no temp files left, server started.
t='download, checksum match'
r=$(make_repo)
mkdir -p "$r/rel/v$version" "$r/data/bin"
make_fake "$r/rel/v$version/$asset" "$r/ran"
sum=$(shasum -a 256 "$r/rel/v$version/$asset" | awk '{print $1}')
printf '# test sums\n%s  %s\n' "$sum" "$asset" > "$r/sums"
make_fake "$r/data/bin/hxprobe-0.0.1" "$r/old-ran"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" HXPROBE_RELEASE_BASE="file://$r/rel" \
    HXPROBE_SUMS="$r/sums" PATH="$nocargo_path" -- --refresh-secs 0
mode=$(stat -f %Lp "$r/data/bin/hxprobe-$version" 2>/dev/null || echo none)
left=$(find "$r/data" -mindepth 1 \( -name '.download.*' -o -name 'hxprobe-0.0.1' \) | wc -l | tr -d ' ')
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp --refresh-secs 0' ] && [ "$mode" = 700 ] \
    && [ "$left" = 0 ] && lock_free "$r/data" \
    && [[ "$err" == *'checksum of '"$asset"' verified'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' mode=$mode left=$left err='$err'"; fi

# (iii-b) A non-file base URL is limited to https: http:// is refused by curl
# before any connection, so the launcher falls through to the final error.
t='download refuses http'
r=$(make_repo)
printf '%s  %s\n' "$sum" "$asset" > "$r/release/SHA256SUMS"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" HXPROBE_RELEASE_BASE="http://127.0.0.1:9" \
    PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *"download of $asset failed"* ]] \
    && [ ! -e "$r/data/bin/hxprobe-$version" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (iii-c) The options that curl is given, recorded by a stub: https only
# (also for redirects), TLS 1.2 or later and time limits; `file` only for a
# file:// base URL.
t='curl options'
r=$(make_repo)
printf '%s  %s\n' "$sum" "$asset" > "$r/release/SHA256SUMS"
mkdir -p "$r/stub"
cat > "$r/stub/curl" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$r/curl-args"
exit 22
EOF
chmod 755 "$r/stub/curl"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$r/stub:$nocargo_path" --
run "$r" CLAUDE_PLUGIN_DATA="$r/data" HXPROBE_RELEASE_BASE="http://127.0.0.1:9" \
    PATH="$r/stub:$nocargo_path" --
run "$r" CLAUDE_PLUGIN_DATA="$r/data" HXPROBE_RELEASE_BASE="file://$r/rel" \
    PATH="$r/stub:$nocargo_path" --
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
    && [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *"download of $asset failed"* ]]; then pass "$t"
else fail "$t" "rc=$rc a1='$a1' a2='$a2' a3='$a3' err='$err'"; fi

# (iii-d) A failed move into place stops the install: nothing is reported as
# installed and no temp file is left.
t='install failure stops'
r=$(make_repo)
mkdir -p "$r/rel/v$version" "$r/stub"
make_fake "$r/rel/v$version/$asset" "$r/ran"
printf '%s  %s\n' "$(shasum -a 256 "$r/rel/v$version/$asset" | awk '{print $1}')" "$asset" > "$r/release/SHA256SUMS"
printf '#!/bin/sh\necho "mv: stub failure" >&2\nexit 1\n' > "$r/stub/mv"
chmod 755 "$r/stub/mv"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" HXPROBE_RELEASE_BASE="file://$r/rel" \
    PATH="$r/stub:$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/ran" ] && [[ "$err" == *'cannot move'* ]] \
    && [[ "$err" != *'installed hxprobe'* ]] && [ ! -e "$r/data/bin/hxprobe-$version" ] \
    && [ "$(leftovers "$r/data")" = 0 ] && lock_free "$r/data"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (iv) Checksum mismatch: the file is not run and not left behind.
t='download, checksum mismatch'
r=$(make_repo)
mkdir -p "$r/rel/v$version"
make_fake "$r/rel/v$version/$asset" "$r/ran"
printf '%s  %s\n' "$(printf '0%.0s' $(seq 64))" "$asset" > "$r/release/SHA256SUMS"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" HXPROBE_RELEASE_BASE="file://$r/rel" \
    PATH="$nocargo_path" --
files=$(find "$r/data/bin" -mindepth 1 | wc -l | tr -d ' ')
if [ "$rc" = 1 ] && [ -z "$out" ] && [ ! -e "$r/ran" ] && [ "$files" = 0 ] \
    && [[ "$err" == *'checksum mismatch'* ]] && lock_free "$r/data"; then pass "$t"
else fail "$t" "rc=$rc out='$out' files=$files err='$err'"; fi

# (v) No cargo and no asset: exit 1 with the fix.
t='no cargo, no asset'
r=$(make_repo)
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *'https://rustup.rs'* ]] \
    && [[ "$err" == *HXPROBE_BIN* ]] && lock_free "$r/data"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (v-b) Missing version in Cargo.toml: clear error.
t='no version'
r=$(make_repo)
printf '[package]\nname = "hxprobe"\n[dependencies]\nversion = "1"\n' > "$r/hxprobe/Cargo.toml"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *'no version found'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (vi) The launcher itself writes nothing to stdout: with a cargo stub that
# prints to stdout, and with a download, stdout holds only the server output.
t='stdout clean'
r=$(make_repo)
make_cargo_stub "$r" ''
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$r/stub:$nocargo_path" -- x
built_ok=0
if [ "$rc" = 0 ] && [ "$out" = 'BUILT mcp x' ] && [[ "$err" == *'cargo noise on stdout'* ]] \
    && [ "$(leftovers "$r/data")" = 0 ] && [ -x "$r/data/bin/hxprobe-$version" ]; then built_ok=1; fi
r2=$(make_repo)
make_fake "$r2/rel-$asset" "$r2/ran"
mkdir -p "$r2/rel/v$version"
mv "$r2/rel-$asset" "$r2/rel/v$version/$asset"
printf '%s  %s\n' "$(shasum -a 256 "$r2/rel/v$version/$asset" | awk '{print $1}')" "$asset" > "$r2/release/SHA256SUMS"
run "$r2" CLAUDE_PLUGIN_DATA="$r2/data" HXPROBE_RELEASE_BASE="file://$r2/rel" PATH="$nocargo_path" --
if [ "$built_ok" = 1 ] && [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ]; then pass "$t"
else fail "$t" "built_ok=$built_ok rc=$rc out='$out' err='$err'"; fi

# (vii) The install lock. A lock directory left by an earlier launcher is
# replaced by the lock file; a held lock is waited for until the holder has
# installed the program.
t='old lock directory'
r=$(make_repo)
mkdir -p "$r/data/install.lock" "$r/data/bin"
dead_pid > "$r/data/install.lock/pid"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [[ "$err" == *'https://rustup.rs'* ]] && [[ "$err" != *'without a lock'* ]] \
    && [ -f "$r/data/install.lock" ] && lock_free "$r/data"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

t='live lock'
r=$(make_repo)
mkdir -p "$r/data/bin"
make_fake "$r/fake" "$r/ran"
lockf -k "$r/data/install.lock" sh -c "sleep 2; cp '$r/fake' '$r/data/bin/hxprobe-$version'" &
helper=$!
i=0
while lock_free "$r/data" && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$nocargo_path" --
wait "$helper" || true
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [[ "$err" == *'waiting'* ]] \
    && lock_free "$r/data"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (viii) Many launchers at once: exactly one build runs and every launcher
# starts the server. Second variant: the lock file and build directories are
# left over from a process that was killed while it held the lock.
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
            mkdir -p "$r/data/bin" "$r/data/target/release" "$r/data/target.$(dead_pid)/release"
            ( exec 9>> "$r/data/install.lock"; lockf -s -t 0 9; exec sleep 30 ) &
            holder=$!
            i=0
            while lock_free "$r/data" && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
            kill -9 "$holder"
            wait "$holder" 2>/dev/null || true
        fi
        run_many "$r" "$many" "$r/stub:$nocargo_path"
        builds=$(wc -l < "$r/builds" | tr -d ' ')
        if [ "$bad" != 0 ] || [ "$builds" != 1 ] || [ "$(leftovers "$r/data")" != 0 ] \
            || ! lock_free "$r/data"; then
            detail="round $round: bad=$bad builds=$builds leftovers=$(leftovers "$r/data") err='$(cat "$r"/err.*)'"
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
if [ "$bad" = 0 ] && [ "$builds" -ge 1 ] && [ "$(leftovers "$r/data")" = 0 ] \
    && [[ "$(cat "$r/err.0")" == *'installing without a lock'* ]]; then pass "$t"
else fail "$t" "bad=$bad builds=$builds leftovers=$(leftovers "$r/data") err='$(cat "$r"/err.*)'"; fi

# (ix) TERM during a build or a download ends the launcher within about 2 s
# with a non-zero status; the lock is free and nothing is left behind.
for what in build download; do
    t="TERM during $what"
    r=$(make_repo)
    mkdir -p "$r/stub"
    if [ "$what" = build ]; then
        printf '#!/bin/sh\n: > "%s"\nexec sleep 30\n' "$r/started" > "$r/stub/cargo"
    else
        printf '%s  %s\n' "$sum" "$asset" > "$r/release/SHA256SUMS"
        printf '#!/bin/sh\n: > "%s"\nexec sleep 30\n' "$r/started" > "$r/stub/curl"
    fi
    chmod 755 "$r/stub"/*
    env -u HXPROBE_BIN -u HXPROBE_RELEASE_BASE -u HXPROBE_SUMS \
        HOME="$root/home" CLAUDE_PLUGIN_DATA="$r/data" PATH="$r/stub:$nocargo_path" \
        bash "$r/scripts/run-mcp.sh" > "$r/out" 2> "$r/err" &
    pid=$!
    i=0
    while [ ! -e "$r/started" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
    before=$(date +%s)
    kill -TERM "$pid"
    rc=0
    wait "$pid" || rc=$?
    took=$(( $(date +%s) - before ))
    if [ -e "$r/started" ] && [ "$rc" = 143 ] && [ "$took" -le 2 ] && [ ! -s "$r/out" ] \
        && [ "$(leftovers "$r/data")" = 0 ] && lock_free "$r/data" \
        && [ ! -e "$r/data/bin/hxprobe-$version" ]; then pass "$t"
    else fail "$t" "rc=$rc took=${took}s leftovers=$(leftovers "$r/data") err='$(cat "$r/err")'"; fi
done

if [ "$failures" -ne 0 ]; then
    printf '%d case(s) failed\n' "$failures"
    exit 1
fi
printf 'all cases passed\n'
