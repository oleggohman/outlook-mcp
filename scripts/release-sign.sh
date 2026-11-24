#!/bin/bash
# Signs a built hxprobe program for a release and prints its checksum line.
#
#   HXPROBE_SIGN_IDENTITY=<identity> scripts/release-sign.sh <program> <version> <arch>
#   scripts/release-sign.sh --compare <signed program>
#
# <arch> is arm64 or x86_64. The signed copy is written to the current
# directory as hxprobe-<version>-macos-<arch>; the input file is not changed.
# The script signs it with the Developer ID identity named by
# HXPROBE_SIGN_IDENTITY (required, never defaulted, never printed), with the
# hardened runtime, a secure timestamp and the fixed signing identifier, then
# checks the result: strict verification; a designated requirement that
# names the identifier, the Apple anchor and the team (and no code hash,
# which would tie a Full Disk Access grant to one build) and equals the code
# requirement of the device profile (see below); a Developer ID
# Application certificate; the runtime flag; the architecture named on the
# command line and, when this machine has it, the version (the program is
# run with --version). Last it prints the line for
# release/SHA256SUMS, computed from the signed file, which is the file to
# publish. Messages go to stderr; stdout holds only that line. What codesign
# prints when it fails is not shown, as it can name the signer; with
# HXPROBE_SIGN_DEBUG=1 it is shown with those names removed.
#
# The comparison with the profile: from what `codesign -d -r-` prints, the
# one line that starts with `designated => ` is taken without that prefix
# (other lines, such as `Executable=...`, are ignored; none or more than one
# is an error). The CodeRequirement of the Full Disk Access entry is read
# from mdm/hxprobe-full-disk-access.mobileconfig (HXPROBE_PROFILE names
# another file). In both, double quotes around the value of
# `certificate leaf[subject.OU] = ` are removed; nothing else is changed.
# The two must then be equal, character for character; on a difference the
# script stops and prints neither. With --compare only this comparison runs,
# on a program signed before: no identity, no signing, nothing written;
# status 0 for a match and 1 otherwise.
#
# Written for the macOS system bash (3.2).

set -euo pipefail

# Never change: the Full Disk Access grants of all users depend on it.
identifier=io.github.oleggohman.hxprobe

here=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
profile=${HXPROBE_PROFILE:-$here/../mdm/hxprobe-full-disk-access.mobileconfig}

log() { printf 'release-sign: %s\n' "$*" >&2; }
die() { log "$*"; exit 1; }
usage() {
    log "usage: HXPROBE_SIGN_IDENTITY=<identity> $0 <program> <version> <arm64|x86_64>"
    log "       $0 --compare <signed program>"
    exit 2
}

# The CodeRequirement of the Full Disk Access entry of the profile.
profile_requirement() {
    local r
    [ -f "$profile" ] || die "no profile file $profile (HXPROBE_PROFILE names another one)"
    r=$(/usr/libexec/PlistBuddy -c 'Print :PayloadContent:0:Services:SystemPolicyAllFiles:0:CodeRequirement' \
        "$profile" 2>/dev/null) || r=''
    [ -n "$r" ] || die "no CodeRequirement in the Full Disk Access entry of $profile"
    printf '%s\n' "$r"
}

# The one line of codesign output $1 that starts with `designated => `,
# without that prefix.
designated_of() {
    local n
    n=$(printf '%s\n' "$1" | awk 'index($0, "designated => ") == 1 { n++ } END { print n + 0 }')
    [ "$n" = 1 ] || { log "codesign printed $n lines that start with 'designated => ', not one"; return 1; }
    printf '%s\n' "$1" | awk 'index($0, "designated => ") == 1 { print substr($0, 15) }'
}

# Requirement $1 without double quotes around the team value.
unquote_team() {
    printf '%s\n' "$1" | sed 's/certificate leaf\[subject\.OU\] = "\([^"]*\)"/certificate leaf[subject.OU] = \1/g'
}

# Compares codesign output $1 with the profile's requirement $2; stops on a
# difference and prints neither.
compare_with_profile() {
    local signed
    signed=$(designated_of "$1") || die "cannot take the designated requirement from what codesign printed"
    [ "$(unquote_team "$signed")" = "$(unquote_team "$2")" ] \
        || die "the designated requirement of the signed program differs from the code requirement in $profile; check the signing identity, or replace the CodeRequirement in the profile"
}

if [ "${1:-}" = --compare ]; then
    [ "$#" = 2 ] || usage
    [ -f "$2" ] || { log "no such file: $2"; usage; }
    preq=$(profile_requirement) || exit 1
    req=$(codesign -d -r- "$2" 2>&1) || die "cannot read the designated requirement"
    compare_with_profile "$req" "$preq"
    log "the designated requirement of $2 matches the code requirement in $profile"
    exit 0
fi

# The text $1 without what tells who signs: the identity, a certificate name
# of any kind (`<kind>: <name> (<team>)`, and `Developer ID Application: ...`
# up to a quote or the end of the line), a team in brackets, the home
# directory and the user name in any other /Users/<name> path.
redact() {
    printf '%s\n' "$1" | HXPROBE_REDACT="$HXPROBE_SIGN_IDENTITY" HXPROBE_REDACT_HOME="${HOME:-}" awk '
        function cut(line, what, with,    out, i) {
            out = ""
            while (what != "" && (i = index(line, what)) > 0) {
                out = out substr(line, 1, i - 1) with
                line = substr(line, i + length(what))
            }
            return out line
        }
        BEGIN {
            id = ENVIRON["HXPROBE_REDACT"]
            home = ENVIRON["HXPROBE_REDACT_HOME"]
            if (home == "/") home = ""
            c = "[A-Z0-9]"
            team = "\\(" c c c c c c c c c c "\\)"
            signer = "[A-Za-z][A-Za-z0-9 ]*: [^\"():]* " team
            user = "/Users/[^/ \"\047:)]+"
        }
        {
            line = cut(cut($0, id, "<identity>"), home, "<home>")
            gsub(signer, "<signer>", line)
            gsub(/Developer ID Application:[^"]*/, "<signer>", line)
            gsub(team, "(<team>)", line)
            gsub(user, "/Users/<user>", line)
            print line
        }'
}

# Stops with message $1 and the status $2 of a codesign run. What codesign
# printed ($3) can name the signer, the team and keychain paths, so it is
# shown only with HXPROBE_SIGN_DEBUG=1, and then through `redact`.
codesign_failed() {
    if [ "${HXPROBE_SIGN_DEBUG:-}" = 1 ]; then
        log "what codesign printed (signer, team, home directory and user names removed):"
        redact "$3" >&2
        die "$1 (status $2)"
    fi
    die "$1 (status $2); run again with HXPROBE_SIGN_DEBUG=1 to see what codesign printed"
}

[ "$#" = 3 ] || usage
prog=$1
version=$2
arch=$3
[ -f "$prog" ] || { log "no such file: $prog"; usage; }
case "$version" in
    '' | *[!0-9A-Za-z.+-]* ) log "bad version '$version'"; usage ;;
esac
case "$arch" in
    arm64 | x86_64 ) ;;
    * ) log "bad arch '$arch'"; usage ;;
esac
if [ -z "${HXPROBE_SIGN_IDENTITY:-}" ]; then
    die "HXPROBE_SIGN_IDENTITY is not set: set it to the Developer ID Application identity to sign with"
fi

asset="hxprobe-$version-macos-$arch"
[ ! -e "$asset" ] || die "$asset already exists in $(pwd -P); remove it first"
# Read before anything is signed, so a missing profile stops at once.
preq=$(profile_requirement) || exit 1

# The copy is signed and checked under a temporary name and gets its final
# name only when every check passed.
tmp=''
cleanup() {
    if [ -n "$tmp" ]; then rm -f "$tmp" 2>/dev/null || true; fi
}
trap cleanup EXIT
tmp=$(mktemp "./.$asset.XXXXXX")
cp "$prog" "$tmp"
# mktemp made the file 0600 and cp keeps the mode of an existing file.
chmod 755 "$tmp"

rc=0
out=$(codesign --force --options runtime --timestamp --identifier "$identifier" \
    --sign "$HXPROBE_SIGN_IDENTITY" "$tmp" 2>&1) || rc=$?
[ "$rc" = 0 ] || codesign_failed "codesign failed" "$rc" "$out"
out=$(codesign --verify --strict --verbose=2 "$tmp" 2>&1) || rc=$?
[ "$rc" = 0 ] || codesign_failed "the signature does not verify" "$rc" "$out"

# codesign prints the requirement and the details on stderr.
req=$(codesign -d -r- "$tmp" 2>&1) || die "cannot read the designated requirement"
case "$req" in
    *cdhash* ) die "the designated requirement contains a cdhash (not a Developer ID signature)" ;;
esac
case "$req" in
    *"identifier \"$identifier\""* ) ;;
    * ) die "the designated requirement does not name identifier \"$identifier\"" ;;
esac
case "$req" in
    *'anchor apple generic'* ) ;;
    * ) die "the designated requirement lacks 'anchor apple generic'" ;;
esac
case "$req" in
    *'certificate leaf[subject.OU]'* ) ;;
    * ) die "the designated requirement lacks a 'certificate leaf[subject.OU]' clause" ;;
esac
compare_with_profile "$req" "$preq"

info=$(codesign -dvv "$tmp" 2>&1) || die "cannot read the signature details"
# The first Authority line is the signing certificate.
leaf=$(printf '%s\n' "$info" | awk '/^Authority=/ { print; exit }')
case "$leaf" in
    'Authority=Developer ID Application:'* ) ;;
    * ) die "not signed with a Developer ID Application certificate" ;;
esac
if ! printf '%s\n' "$info" \
        | grep -Eq '^CodeDirectory .*flags=0x[0-9a-f]+\(([a-z-]+,)*runtime(,[a-z-]+)*\)'; then
    die "the hardened runtime flag (runtime) is not set"
fi

# The file must be what its name says: a program of that architecture only
# and, where this machine can run it, of that version.
archs=$(lipo -archs "$tmp" 2>/dev/null) || die "cannot read the architecture of the program (lipo)"
[ "$archs" = "$arch" ] \
    || die "the program has architecture '$archs', not $arch as the name of the asset says"
if [ "$(uname -m)" = "$arch" ]; then
    said=$("$tmp" --version </dev/null 2>/dev/null) || said=''
    [ "$said" = "hxprobe $version" ] \
        || die "the program reports version '${said#hxprobe }', not $version as the name of the asset says"
else
    log "not asking the program for its version: this machine is not $arch"
fi

# The checksum is taken before the file gets its final name, so a failure
# here leaves nothing that looks like a finished asset.
sum=$(shasum -a 256 "$tmp" | awk '{ print tolower($1) }') || sum=''
case "$sum" in
    *[!0-9a-f]* | '' ) die "cannot compute the SHA-256 of $asset" ;;
esac
[ "${#sum}" = 64 ] || die "cannot compute the SHA-256 of $asset"
mv -f "$tmp" "$asset"
tmp=''
log "signed and checked $asset; add this line to release/SHA256SUMS:"
printf '%s  %s\n' "$sum" "$asset"
