#!/bin/bash
# Tests for scripts/release-sign.sh. No signing identity and no keychain:
# `codesign` and `shasum` are stubs on PATH. The codesign stub records its
# arguments, prints its answers on stderr as the real one does, and appends
# bytes to the file it signs, so the checksum must be that of the changed
# file.

set -euo pipefail

here=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
script="$here/release-sign.sh"
ident='io.github.oleggohman.hxprobe'
# A made-up identity; it must never appear in the script's output.
identity='Stub Identity For Tests QX7'

root=$(mktemp -d "${TMPDIR:-/tmp}/test-release-sign.XXXXXX")
trap 'rm -rf "$root"' EXIT

failures=0
pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s: %s\n' "$1" "$2"; failures=$((failures + 1)); }

good_req="designated => identifier \"$ident\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = STUBTEAM"
good_info="Executable=/x/hxprobe
Identifier=$ident
Format=Mach-O thin (arm64)
CodeDirectory v=20500 size=1234 flags=0x10000(runtime) hashes=30+7 location=embedded
Signature size=9000
Authority=Developer ID Application: Stub Person (STUBTEAM)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
Timestamp=1 Jan 2026 at 00:00:00
TeamIdentifier=STUBTEAM"

# A case directory with the stubs in $1/stub and the answers of codesign in
# $1/req (designated requirement), $1/info (-dvv), $1/verify-rc and
# $1/sign-rc; $1/sign-err is what the stub prints when signing fails. The
# `lipo` stub prints $1/archs and the `uname` stub $1/host-arch for `-m`;
# the program is a script that answers --version with $1/prog-version.
make_case() {
    local c
    c=$(mktemp -d "$root/case.XXXXXX")
    mkdir -p "$c/stub" "$c/work"
    printf '%s\n' "$good_req" > "$c/req"
    printf '%s\n' "$good_info" > "$c/info"
    echo 0 > "$c/verify-rc"
    echo 0 > "$c/sign-rc"
    : > "$c/sign-err"
    cat > "$c/stub/codesign" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$c/codesign-args"
for last; do :; done
case " \$* " in
    *' --sign '*)
        rc=\$(cat "$c/sign-rc")
        if [ "\$rc" != 0 ]; then cat "$c/sign-err" >&2; exit "\$rc"; fi
        printf 'SIGNED' >> "\$last"
        echo "\$last: replacing existing signature" >&2
        exit 0 ;;
    *' --verify '*)
        echo "\$last: valid on disk" >&2
        exit \$(cat "$c/verify-rc") ;;
    *' -r- '*)
        echo "Executable=\$last" >&2
        cat "$c/req" >&2
        exit 0 ;;
    *' -dvv '*)
        cat "$c/info" >&2
        exit 0 ;;
esac
echo "codesign stub: unexpected arguments" >&2
exit 64
EOF
    cat > "$c/stub/shasum" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$c/shasum-args"
exec /usr/bin/shasum "\$@"
EOF
    echo arm64 > "$c/archs"
    echo arm64 > "$c/host-arch"
    echo 'hxprobe 9.8.7' > "$c/prog-version"
    cat > "$c/stub/lipo" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$c/lipo-args"
cat "$c/archs"
EOF
    cat > "$c/stub/uname" <<EOF
#!/bin/sh
if [ "\$*" = -m ]; then cat "$c/host-arch"; exit 0; fi
exec /usr/bin/uname "\$@"
EOF
    chmod 755 "$c/stub/codesign" "$c/stub/shasum" "$c/stub/lipo" "$c/stub/uname"
    cat > "$c/prog" <<EOF
#!/bin/sh
if [ "\$1" = --version ]; then cat "$c/prog-version"; exit 0; fi
exit 2
EOF
    cp "$c/prog" "$c/prog.orig"
    printf '%s\n' "$c"
}

# Runs the script in $1/work with the script arguments $3...; $2 is the
# identity: `unset`, `empty` or `set` (the made-up one). Sets out, err, rc.
run() {
    local c=$1 mode=$2
    shift 2
    rc=0
    (
        cd "$c/work"
        unset HXPROBE_SIGN_IDENTITY
        case "$mode" in
            empty ) export HXPROBE_SIGN_IDENTITY='' ;;
            set ) export HXPROBE_SIGN_IDENTITY="$identity" ;;
        esac
        PATH="$c/stub:/usr/bin:/bin" bash "$script" "$@" > "$c/out" 2> "$c/err"
    ) || rc=$?
    out=$(cat "$c/out")
    err=$(cat "$c/err")
}

# Runs with the made-up identity and the usual arguments.
run_signed() {
    run "$1" set "$1/prog" 9.8.7 arm64
}

# Succeeds when the identity appears nowhere in the script's output of case $1.
no_identity() {
    ! grep -qF "$identity" "$1/out" "$1/err"
}

# Number of files in the work directory of case $1.
work_files() {
    find "$1/work" -mindepth 1 | wc -l | tr -d ' '
}

asset=hxprobe-9.8.7-macos-arm64

# (i) Without HXPROBE_SIGN_IDENTITY (unset or empty) the script refuses to
# run and calls no codesign.
t='no identity'
c=$(make_case)
run "$c" unset "$c/prog" 9.8.7 arm64
rc1=$rc; err1=$err
run "$c" empty "$c/prog" 9.8.7 arm64
if [ "$rc1" != 0 ] && [ "$rc" != 0 ] && [[ "$err1" == *HXPROBE_SIGN_IDENTITY* ]] \
    && [[ "$err" == *HXPROBE_SIGN_IDENTITY* ]] && [ -z "$out" ] \
    && [ ! -e "$c/codesign-args" ] && [ "$(work_files "$c")" = 0 ]; then pass "$t"
else fail "$t" "rc=$rc1/$rc err='$err1' / '$err'"; fi

# (ii) Wrong arguments: usage and status 2, nothing signed.
t='usage'
c=$(make_case)
run "$c" set "$c/prog" 9.8.7
rc1=$rc
run "$c" set "$c/missing" 9.8.7 arm64
rc2=$rc
run "$c" set "$c/prog" 9.8.7 sparc
rc3=$rc
run "$c" set "$c/prog" '9.8.7/x' arm64
if [ "$rc1" = 2 ] && [ "$rc2" = 2 ] && [ "$rc3" = 2 ] && [ "$rc" = 2 ] \
    && [[ "$err" == *usage* ]] && [ ! -e "$c/codesign-args" ] \
    && [ "$(work_files "$c")" = 0 ]; then pass "$t"
else fail "$t" "rc=$rc1/$rc2/$rc3/$rc err='$err'"; fi

# (iii) Success: codesign is called with the hardened runtime, a secure
# timestamp, the fixed identifier and the identity from the variable; the
# printed line is the checksum of the signed file; the input is untouched;
# the identity is not in the output; no temp file is left.
t='signed and checksummed'
c=$(make_case)
run_signed "$c"
sign_args=$(grep -e '--sign' "$c/codesign-args" || true)
want=$({ /usr/bin/shasum -a 256 "$c/work/$asset" 2>/dev/null || true; } | awk '{print $1}')
signed=$(tail -c 6 "$c/work/$asset" 2>/dev/null || true)
if [ "$rc" = 0 ] && [ "$out" = "$want  $asset" ] && [ "$signed" = SIGNED ] \
    && [[ " $sign_args " == *' --force '* ]] && [[ " $sign_args " == *' --options runtime '* ]] \
    && [[ " $sign_args " == *' --timestamp '* ]] \
    && [[ " $sign_args " == *" --identifier $ident "* ]] \
    && [[ " $sign_args " == *" --sign $identity "* ]] \
    && grep -q -e '--verify --strict --verbose=2' "$c/codesign-args" \
    && grep -q -e '-d -r-' "$c/codesign-args" && grep -q -e '-dvv' "$c/codesign-args" \
    && grep -q "$asset" "$c/shasum-args" \
    && cmp -s "$c/prog" "$c/prog.orig" && no_identity "$c" \
    && [ "$(stat -f %Lp "$c/work/$asset" 2>/dev/null || true)" = 755 ] \
    && [ "$(work_files "$c")" = 1 ]; then pass "$t"
else fail "$t" "rc=$rc out='$out' want='$want' args='$(cat "$c/codesign-args" 2>/dev/null || true)' err='$err'"; fi

# (iv) Each failed check stops the script: status 1, a message that names
# the check, no checksum line, no output file and no temp file, the input
# untouched, and the identity not in the output.
check_stops() {
    # $1 = name, $2 = case dir, $3 = text the message must contain
    run_signed "$2"
    if [ "$rc" = 1 ] && [ -z "$out" ] && [[ "$err" == *"$3"* ]] \
        && [ "$(work_files "$2")" = 0 ] && cmp -s "$2/prog" "$2/prog.orig" \
        && no_identity "$2"; then pass "$1"
    else fail "$1" "rc=$rc out='$out' err='$err' files=$(work_files "$2")"; fi
}

c=$(make_case)
echo 1 > "$c/verify-rc"
check_stops 'verify fails' "$c" 'does not verify'

c=$(make_case)
printf 'designated => cdhash H"0123456789abcdef0123456789abcdef01234567"\n' > "$c/req"
check_stops 'requirement with cdhash' "$c" 'cdhash'

c=$(make_case)
printf '%s and cdhash H"01"\n' "$good_req" > "$c/req"
check_stops 'requirement with cdhash among the clauses' "$c" 'cdhash'

c=$(make_case)
printf '%s\n' "${good_req/identifier \"$ident\"/identifier \"hxprobe\"}" > "$c/req"
check_stops 'requirement without the identifier' "$c" "$ident"

c=$(make_case)
printf '%s\n' "${good_req/anchor apple generic/anchor trusted}" > "$c/req"
check_stops 'requirement without anchor apple generic' "$c" 'anchor apple generic'

c=$(make_case)
printf '%s\n' "${good_req/ and certificate leaf\[subject.OU\] = STUBTEAM/}" > "$c/req"
check_stops 'requirement without the team clause' "$c" 'subject.OU'

c=$(make_case)
printf '%s\n' "${good_info//Authority=Developer ID Application: Stub Person (STUBTEAM)/Authority=Stub Self Signed}" > "$c/info"
check_stops 'self-signed certificate' "$c" 'Developer ID Application'

c=$(make_case)
printf '%s\n' "${good_info//Authority=Developer ID Application: Stub Person (STUBTEAM)/Authority=Apple Development: Stub Person (STUBTEAM)}" > "$c/info"
check_stops 'development certificate' "$c" 'Developer ID Application'

c=$(make_case)
printf '%s\n' "${good_info//flags=0x10000(runtime)/flags=0x0(none)}" > "$c/info"
check_stops 'no hardened runtime' "$c" 'runtime'

# (iv-b) The file must be what its name says: a thin program of the named
# architecture that, where this machine can run it, reports the version.
c=$(make_case)
echo x86_64 > "$c/archs"
check_stops 'other architecture than named' "$c" 'architecture'

c=$(make_case)
echo 'x86_64 arm64' > "$c/archs"
check_stops 'universal program' "$c" 'architecture'

c=$(make_case)
echo 'hxprobe 9.8.6' > "$c/prog-version"
check_stops 'other version than named' "$c" 'version'

c=$(make_case)
: > "$c/prog-version"
check_stops 'program without a version' "$c" 'version'

# On a machine of another architecture the program cannot be asked.
t='version not asked on another architecture'
c=$(make_case)
echo x86_64 > "$c/host-arch"
echo 'hxprobe 9.8.6' > "$c/prog-version"
run_signed "$c"
if [ "$rc" = 0 ] && [[ "$out" == *"  $asset" ]] && [ -e "$c/work/$asset" ] \
    && grep -q -e '-archs' "$c/lipo-args"; then pass "$t"
else fail "$t" "rc=$rc out='$out' err='$err'"; fi

# (v) A failing codesign that names the identity in its error: the script
# stops and its output holds the error without the identity.
c=$(make_case)
echo 1 > "$c/sign-rc"
printf 'error: %s: no identity found\n' "$identity" > "$c/sign-err"
check_stops 'codesign fails, identity hidden' "$c" 'no identity found'

if [ "$failures" -ne 0 ]; then
    printf '%d case(s) failed\n' "$failures"
    exit 1
fi
printf 'all cases passed\n'
