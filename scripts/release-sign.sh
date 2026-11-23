#!/bin/bash
# Signs a built hxprobe program for a release and prints its checksum line.
#
#   HXPROBE_SIGN_IDENTITY=<identity> scripts/release-sign.sh <program> <version> <arch>
#
# <arch> is arm64 or x86_64. The signed copy is written to the current
# directory as hxprobe-<version>-macos-<arch>; the input file is not changed.
# The script signs it with the Developer ID identity named by
# HXPROBE_SIGN_IDENTITY (required, never defaulted, never printed), with the
# hardened runtime, a secure timestamp and the fixed signing identifier, then
# checks the result: strict verification; a designated requirement that
# names the identifier, the Apple anchor and the team (and no code hash,
# which would tie a Full Disk Access grant to one build); a Developer ID
# Application certificate; the runtime flag. Last it prints the line for
# release/SHA256SUMS, computed from the signed file, which is the file to
# publish. Messages go to stderr; stdout holds only that line.
#
# Written for the macOS system bash (3.2).

set -euo pipefail

# Never change: the Full Disk Access grants of all users depend on it.
identifier=io.github.oleggohman.hxprobe

log() { printf 'release-sign: %s\n' "$*" >&2; }
die() { log "$*"; exit 1; }
usage() {
    log "usage: HXPROBE_SIGN_IDENTITY=<identity> $0 <program> <version> <arm64|x86_64>"
    exit 2
}

# The text $1 with every occurrence of the identity replaced, so that no
# output of this script shows it.
redact() {
    printf '%s\n' "$1" | HXPROBE_REDACT="$HXPROBE_SIGN_IDENTITY" awk '
        BEGIN { id = ENVIRON["HXPROBE_REDACT"] }
        {
            line = $0; out = ""
            while (id != "" && (i = index(line, id)) > 0) {
                out = out substr(line, 1, i - 1) "<identity>"
                line = substr(line, i + length(id))
            }
            print out line
        }'
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

if ! out=$(codesign --force --options runtime --timestamp --identifier "$identifier" \
        --sign "$HXPROBE_SIGN_IDENTITY" "$tmp" 2>&1); then
    die "codesign failed: $(redact "$out")"
fi
if ! out=$(codesign --verify --strict --verbose=2 "$tmp" 2>&1); then
    die "the signature does not verify: $(redact "$out")"
fi

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

mv -f "$tmp" "$asset"
tmp=''
sum=$(shasum -a 256 "$asset" | awk '{ print tolower($1) }')
case "$sum" in
    *[!0-9a-f]* | '' ) die "cannot compute the SHA-256 of $asset" ;;
esac
[ "${#sum}" = 64 ] || die "cannot compute the SHA-256 of $asset"
log "signed and checked $asset; add this line to release/SHA256SUMS:"
printf '%s  %s\n' "$sum" "$asset"
