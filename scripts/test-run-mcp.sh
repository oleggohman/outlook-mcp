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
left=$(find "$r/data" -mindepth 1 \( -name '.download.*' -o -name install.lock -o -name 'hxprobe-0.0.1' \) | wc -l | tr -d ' ')
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp --refresh-secs 0' ] && [ "$mode" = 700 ] \
    && [ "$left" = 0 ] && [[ "$err" == *'checksum of '"$asset"' verified'* ]]; then pass "$t"
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
    && [[ "$err" == *'checksum mismatch'* ]] && [ ! -e "$r/data/install.lock" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' files=$files err='$err'"; fi

# (v) No cargo and no asset: exit 1 with the fix.
t='no cargo, no asset'
r=$(make_repo)
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *'https://rustup.rs'* ]] \
    && [[ "$err" == *HXPROBE_BIN* ]] && [ ! -e "$r/data/install.lock" ]; then pass "$t"
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
mkdir -p "$r/stub" "$r/rel/v$version"
cat > "$r/stub/cargo" <<'EOF'
#!/bin/sh
echo "cargo noise on stdout"
# Locate --target-dir and leave a program where cargo would.
while [ $# -gt 0 ]; do
    if [ "$1" = --target-dir ]; then t=$2; fi
    shift
done
mkdir -p "$t/release"
printf '#!/bin/sh\necho "BUILT $*"\n' > "$t/release/hxprobe"
chmod 755 "$t/release/hxprobe"
EOF
chmod 755 "$r/stub/cargo"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$r/stub:$nocargo_path" -- x
built_ok=0
if [ "$rc" = 0 ] && [ "$out" = 'BUILT mcp x' ] && [[ "$err" == *'cargo noise on stdout'* ]] \
    && [ ! -e "$r/data/target" ] && [ -x "$r/data/bin/hxprobe-$version" ]; then built_ok=1; fi
r2=$(make_repo)
make_fake "$r2/rel-$asset" "$r2/ran"
mkdir -p "$r2/rel/v$version"
mv "$r2/rel-$asset" "$r2/rel/v$version/$asset"
printf '%s  %s\n' "$(shasum -a 256 "$r2/rel/v$version/$asset" | awk '{print $1}')" "$asset" > "$r2/release/SHA256SUMS"
run "$r2" CLAUDE_PLUGIN_DATA="$r2/data" HXPROBE_RELEASE_BASE="file://$r2/rel" PATH="$nocargo_path" --
if [ "$built_ok" = 1 ] && [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ]; then pass "$t"
else fail "$t" "built_ok=$built_ok rc=$rc out='$out' err='$err'"; fi

# (vii) A stale lock (owner gone) is broken; a lock of a live owner is waited
# for until the owner installs the program.
t='stale lock'
r=$(make_repo)
mkdir -p "$r/data/install.lock" "$r/data/bin"
sh -c 'exit 0' & dead=$!; wait "$dead"
printf '%s\n' "$dead" > "$r/data/install.lock/pid"
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$nocargo_path" --
if [ "$rc" = 1 ] && [[ "$err" == *'removing a stale install lock'* ]] \
    && [ ! -e "$r/data/install.lock" ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

t='live lock'
r=$(make_repo)
mkdir -p "$r/data/install.lock" "$r/data/bin"
printf '%s\n' "$$" > "$r/data/install.lock/pid"
make_fake "$r/fake" "$r/ran"
(
    sleep 2
    cp "$r/fake" "$r/data/bin/hxprobe-$version"
    rm -rf "$r/data/install.lock"
) &
helper=$!
run "$r" CLAUDE_PLUGIN_DATA="$r/data" PATH="$nocargo_path" --
wait "$helper" || true
if [ "$rc" = 0 ] && [ "$out" = 'FAKE mcp' ] && [[ "$err" == *'waiting'* ]]; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

if [ "$failures" -ne 0 ]; then
    printf '%d case(s) failed\n' "$failures"
    exit 1
fi
printf 'all cases passed\n'
