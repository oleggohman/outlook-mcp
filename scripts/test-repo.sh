#!/bin/bash
# Repository checks that need no network and no signing identity.
#
#   bash scripts/test-repo.sh            the checks below
#   bash scripts/test-repo.sh --release  the same, and the release gate: the
#                                        checksum file lists the arm64
#                                        program of the version in
#                                        hxprobe/Cargo.toml (HXPROBE_SUMS
#                                        names another checksum file, as in
#                                        the launcher)
#
# The committed device profile mdm/hxprobe-full-disk-access.mobileconfig is
# a valid property list; it grants Full Disk Access to the launcher's
# default program path (read from scripts/run-mcp.sh) by path; its code
# requirement names the signing identifier of scripts/release-sign.sh, the
# Apple anchor and a team, and no code hash; the team value, read from the
# profile at run time, occurs in no other tracked file. The versions of
# hxprobe/Cargo.toml, its lock file and the plugin agree, and CHANGELOG.md
# has that version on top with a compare link. The skill and README.md
# name the warning, the shared program path, the denial and the clients,
# and name the per-user program path only under the headings that explain
# it. Every check is a
# function that takes the file to check; each one also runs on a changed
# copy in a temp directory, where it must fail. The release gate stays out
# of the plain run, so the tree is green before the program is signed.
#
# Reads tracked files only and writes nothing outside its temp directory.
# No message prints the code requirement or the team value. Written for the
# macOS system bash (3.2).

set -euo pipefail

here=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
repo=$(CDPATH='' cd "$here/.." && pwd -P)

release=0
case "$#:${1:-}" in
    0: ) ;;
    1:--release ) release=1 ;;
    * ) printf 'usage: %s [--release]\n' "$0" >&2; exit 2 ;;
esac

profile_rel=mdm/hxprobe-full-disk-access.mobileconfig
profile="$repo/$profile_rel"
launcher="$repo/scripts/run-mcp.sh"
signer="$repo/scripts/release-sign.sh"
cargo_toml="$repo/hxprobe/Cargo.toml"
cargo_lock="$repo/hxprobe/Cargo.lock"
plugin_json="$repo/.claude-plugin/plugin.json"
pb=/usr/libexec/PlistBuddy
# The one Full Disk Access entry of the profile.
entry=PayloadContent:0:Services:SystemPolicyAllFiles:0

tmp=$(mktemp -d "${TMPDIR:-/tmp}/test-repo.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

failures=0
pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s: %s\n' "$1" "$2"; failures=$((failures + 1)); }

# The value at key path $2 of property list $1; fails when it is missing.
pget() { "$pb" -c "Print :$2" "$1" 2>/dev/null; }

# The checks. Each takes the file to check, succeeds or fails, and on
# failure leaves the reason in `why`.
why=''

check_lint() {
    plutil -lint -s "$1" >/dev/null 2>&1 || { why='plutil -lint fails'; return 1; }
}

# One payload with one Full Disk Access entry, so the checks below read the
# only one there is.
check_one_entry() {
    pget "$1" "$entry" >/dev/null || { why='no Full Disk Access entry'; return 1; }
    ! pget "$1" PayloadContent:1 >/dev/null || { why='more than one payload'; return 1; }
    ! pget "$1" PayloadContent:0:Services:SystemPolicyAllFiles:1 >/dev/null \
        || { why='more than one Full Disk Access entry'; return 1; }
}

check_identifier() {
    local got
    got=$(pget "$1" "$entry:Identifier") || got=''
    [ "$got" = "$program" ] || { why="Identifier is '$got', not '$program'"; return 1; }
}

check_identifier_type() {
    local got
    got=$(pget "$1" "$entry:IdentifierType") || got=''
    [ "$got" = path ] || { why="IdentifierType is '$got', not 'path'"; return 1; }
}

# Authorization Allow, and not the older key Allowed besides it.
check_authorization() {
    local got
    got=$(pget "$1" "$entry:Authorization") || got=''
    [ "$got" = Allow ] || { why="Authorization is '$got', not 'Allow'"; return 1; }
    ! pget "$1" "$entry:Allowed" >/dev/null || { why='an Allowed key besides Authorization'; return 1; }
}

check_payload_type() {
    local inner outer
    inner=$(pget "$1" PayloadContent:0:PayloadType) || inner=''
    outer=$(pget "$1" PayloadType) || outer=''
    [ "$inner" = com.apple.TCC.configuration-profile-policy ] \
        || { why="the inner PayloadType is '$inner'"; return 1; }
    [ "$outer" = Configuration ] || { why="the outer PayloadType is '$outer'"; return 1; }
}

check_scope() {
    local got
    got=$(pget "$1" PayloadScope) || got=''
    [ "$got" = System ] || { why="PayloadScope is '$got', not 'System'"; return 1; }
}

# The inner and the outer payload have their own UUID and identifier.
check_distinct() {
    local k a b
    for k in PayloadUUID PayloadIdentifier; do
        a=$(pget "$1" "$k") || a=''
        b=$(pget "$1" "PayloadContent:0:$k") || b=''
        [ -n "$a" ] && [ -n "$b" ] || { why="$k missing"; return 1; }
        [ "$a" != "$b" ] || { why="the inner and the outer $k are equal"; return 1; }
    done
}

# The code requirement names the signing identifier, the Apple anchor and
# a team, and no code hash (which would tie the grant to one build).
check_requirement() {
    local req
    req=$(pget "$1" "$entry:CodeRequirement") || { why='no CodeRequirement'; return 1; }
    case "$req" in
        *"identifier \"$ident\""* ) ;;
        * ) why="the code requirement does not name identifier \"$ident\""; return 1 ;;
    esac
    case "$req" in
        *'anchor apple generic'* ) ;;
        * ) why="the code requirement lacks 'anchor apple generic'"; return 1 ;;
    esac
    case "$req" in
        *'certificate leaf[subject.OU]'* ) ;;
        * ) why="the code requirement lacks a 'certificate leaf[subject.OU]' clause"; return 1 ;;
    esac
    case "$req" in
        *cdhash* ) why='the code requirement contains a cdhash'; return 1 ;;
    esac
}

# The team value of the code requirement in profile $1 (the value after
# `certificate leaf[subject.OU] = `, with or without double quotes), or
# nothing. Only ever captured, never printed by a case.
team_of() {
    { pget "$1" "$entry:CodeRequirement" || true; } \
        | sed -n 's/.*certificate leaf\[subject\.OU\] = "\{0,1\}\([A-Za-z0-9]\{1,\}\)"\{0,1\}.*/\1/p' \
        | head -n 1
}

# The team value of profile $1 occurs in none of the files $2...
check_team_alone() {
    local p=$1 team f
    shift
    team=$(team_of "$p")
    [ -n "$team" ] || { why='no team value in the code requirement'; return 1; }
    for f in "$@"; do
        if grep -qF -e "$team" "$f"; then
            why="the team value of the profile occurs in ${f#"$repo"/}"
            return 1
        fi
    done
}

# The version in the [package] section of Cargo.toml $1.
cargo_version() {
    awk '/^\[/ { p = ($0 == "[package]") } p && /^version *= *"/ { split($0, v, "\""); print v[2]; exit }' "$1"
}

# The version of package hxprobe in Cargo.lock $1.
lock_version() {
    awk '/^\[\[package\]\]/ { p = 0 } $0 == "name = \"hxprobe\"" { p = 1 }
        p && /^version *= *"/ { split($0, v, "\""); print v[2]; exit }' "$1"
}

# The version in hxprobe/Cargo.toml $1, the hxprobe package of Cargo.lock
# $2 and the plugin manifest $3 are one and the same.
check_versions() {
    local toml lock plugin
    toml=$(cargo_version "$1")
    lock=$(lock_version "$2")
    plugin=$(plutil -extract version raw -o - "$3" 2>/dev/null) || plugin=''
    [ -n "$toml" ] || { why='no version in hxprobe/Cargo.toml'; return 1; }
    [ "$toml" = "$lock" ] && [ "$toml" = "$plugin" ] \
        || { why="Cargo.toml '$toml', Cargo.lock '$lock', plugin.json '$plugin'"; return 1; }
}

# File $1 contains each of the fixed strings $2...
check_phrases() {
    local f=$1 p
    shift
    for p in "$@"; do
        grep -qF -e "$p" "$f" || { why="$(basename "$f") lacks '$p'"; return 1; }
    done
}

# The description in the front matter of skill $1 is a "Use when ..."
# trigger list that names a warning in a result.
check_skill_description() {
    local d
    d=$(awk 'NR == 1 && $0 != "---" { exit } NR > 1 && $0 == "---" { exit }
        NR > 1 && /^description:/ { sub(/^description: */, ""); print }' "$1")
    case "$d" in
        'Use when '* ) ;;
        * ) why='the description does not start with "Use when "'; return 1 ;;
    esac
    case "$d" in
        *warning* ) ;;
        * ) why='the description does not name a warning in a result'; return 1 ;;
    esac
}

# Every line of Markdown file $1 that contains the fixed string $2 lies
# under one of the headings $3... (any level, also below a subheading of
# it). Lines in fenced code blocks are never taken for headings.
check_lines_under() {
    local f=$1 s=$2 bad
    shift 2
    # The headings reach awk through the environment: `-v` cannot carry a
    # line break in the macOS awk.
    bad=$(HXPROBE_HEADINGS="$(printf '%s\n' "$@")" awk -v s="$s" '
        BEGIN { n = split(ENVIRON["HXPROBE_HEADINGS"], a, "\n"); for (i = 1; i <= n; i++) ok[a[i]] = 1 }
        /^```/ { fence = !fence }
        !fence && /^#+ / {
            l = index($0, " ") - 1
            h[l] = substr($0, l + 2)
            for (i = l + 1; i <= 6; i++) delete h[i]
            next
        }
        index($0, s) {
            good = 0
            for (i = 1; i <= 6; i++) if ((i in h) && (h[i] in ok)) good = 1
            if (!good) { print NR; exit }
        }' "$f") || { why="awk failed on $(basename "$f")"; return 1; }
    [ -z "$bad" ] || { why="line $bad of $(basename "$f") names '$s' outside the allowed headings"; return 1; }
}

# Changelog $1: the newest section is `## [$2] - <date>`, the next one is the
# previous version, and a link `[$2]: .../compare/v<previous>...v$2` exists.
check_changelog() {
    local top prev
    top=$(sed -n 's/^## \[\([^]]*\)\] - [0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}$/\1/p' "$1" | head -n 1)
    prev=$(sed -n 's/^## \[\([^]]*\)\] - .*/\1/p' "$1" | sed -n 2p)
    [ "$top" = "$2" ] || { why="the newest section of $(basename "$1") is '$top', not '$2'"; return 1; }
    [ "$(sed -n 's/^## \[\([^]]*\)\].*/\1/p' "$1" | head -n 1)" = "$2" ] \
        || { why="the first section of $(basename "$1") is not '## [$2] - <date>'"; return 1; }
    [ -n "$prev" ] || { why="no section before $2 in $(basename "$1")"; return 1; }
    grep -qF -e "[$2]: https://github.com/oleggohman/outlook-mcp/compare/v$prev...v$2" "$1" \
        || { why="no compare link [$2] from v$prev in $(basename "$1")"; return 1; }
}

# Checksum file $1 lists hxprobe-$2-macos-arm64 with a 64-digit sum. Read
# as the launcher reads it: `#` comments and blank lines are skipped, and
# the `*` that shasum may put before a name is not part of the name.
check_release_sums() {
    local a="hxprobe-$2-macos-arm64" got
    [ -f "$1" ] || { why="no checksum file $1"; return 1; }
    got=$(awk -v a="$a" '
        /^[[:space:]]*#/ || NF == 0 { next }
        NF == 2 { n = $2; sub(/^\*/, "", n); if (n == a) { print tolower($1); exit } }
    ' "$1")
    case "$got" in
        '' ) why="$(basename "$1") has no line for $a: add the line that scripts/release-sign.sh printed"; return 1 ;;
        *[!0-9a-f]* ) why="the sum for $a in $(basename "$1") is not hexadecimal"; return 1 ;;
    esac
    [ "${#got}" = 64 ] || { why="the sum for $a in $(basename "$1") does not have 64 digits"; return 1; }
}

# Runs check $2 on the real file(s) $3...: case $1 must pass.
expect_pass() {
    local t=$1
    shift
    if "$@"; then pass "$t"; else fail "$t" "$why"; fi
}
# Runs check $2 on the changed file(s) $3...: case $1 must fail.
expect_fail() {
    local t=$1
    shift
    if "$@"; then fail "$t" 'the check passed on a changed copy'; else pass "$t"; fi
}

# A copy of the profile in the temp directory, named $1, changed by the
# PlistBuddy commands $2...; prints its path.
changed() {
    local c="$tmp/$1.mobileconfig" cmd
    shift
    cp "$profile" "$c"
    for cmd in "$@"; do "$pb" -c "$cmd" "$c" >/dev/null; done
    printf '%s\n' "$c"
}

# A copy of the profile named $1 whose code requirement is $2.
with_req() {
    local c
    c=$(changed "$1")
    plutil -replace "${entry//:/.}.CodeRequirement" -string "$2" "$c"
    printf '%s\n' "$c"
}

# The inputs are tracked files.
t='inputs are tracked'
missing=''
for f in "$profile_rel" scripts/run-mcp.sh scripts/release-sign.sh hxprobe/Cargo.toml \
    hxprobe/Cargo.lock .claude-plugin/plugin.json skills/outlook-local-mail/SKILL.md \
    README.md CHANGELOG.md; do
    git -C "$repo" ls-files --error-unmatch -- "$f" >/dev/null 2>&1 || missing="$missing $f"
done
if [ -z "$missing" ]; then pass "$t"; else fail "$t" "not tracked:$missing"; fi

# What the profile must agree with, read from the scripts themselves.
shared=$(sed -n 's/^shared_default=//p' "$launcher")
program="$shared/bin/hxprobe"
ident=$(sed -n 's/^identifier=//p' "$signer")
t='launcher path and signing identifier found'
if [ -n "$shared" ] && [ "$(printf '%s\n' "$shared" | wc -l | tr -d ' ')" = 1 ] \
    && [ -n "$ident" ] && [ "$(printf '%s\n' "$ident" | wc -l | tr -d ' ')" = 1 ]; then pass "$t"
else fail "$t" "shared_default='$shared' identifier='$ident'"; fi

# The committed profile passes every check.
expect_pass 'profile: plutil -lint' check_lint "$profile"
expect_pass 'profile: one payload, one entry' check_one_entry "$profile"
expect_pass 'profile: Identifier is the launcher program path' check_identifier "$profile"
expect_pass 'profile: IdentifierType path' check_identifier_type "$profile"
expect_pass 'profile: Authorization Allow, no Allowed' check_authorization "$profile"
expect_pass 'profile: payload types' check_payload_type "$profile"
expect_pass 'profile: scope System' check_scope "$profile"
expect_pass 'profile: payload UUIDs and identifiers differ' check_distinct "$profile"
expect_pass 'profile: code requirement' check_requirement "$profile"

# The team value occurs in no other tracked file (this script included).
others=()
while IFS= read -r -d '' f; do
    [ "$f" = "$profile_rel" ] && continue
    [ -f "$repo/$f" ] && others+=("$repo/$f")
done < <(git -C "$repo" ls-files -z)
expect_pass 'profile: team value in no other tracked file' \
    check_team_alone "$profile" ${others[@]+"${others[@]}"}

# Self-test: each check fails on a changed copy.
broken="$tmp/broken.mobileconfig"
head -n 10 "$profile" > "$broken"
expect_fail 'self-test: lint, truncated file' check_lint "$broken"
expect_fail 'self-test: a second entry' check_one_entry \
    "$(changed second-entry "Copy :$entry :PayloadContent:0:Services:SystemPolicyAllFiles:1")"
expect_fail 'self-test: a second payload' check_one_entry \
    "$(changed second-payload 'Copy :PayloadContent:0 :PayloadContent:1')"
expect_fail 'self-test: wrong path' check_identifier \
    "$(changed wrong-path "Set :$entry:Identifier /Applications/hxprobe")"
expect_fail 'self-test: bundleID' check_identifier_type \
    "$(changed bundle-id "Set :$entry:IdentifierType bundleID")"
expect_fail 'self-test: Deny' check_authorization \
    "$(changed deny "Set :$entry:Authorization Deny")"
expect_fail 'self-test: an Allowed key' check_authorization \
    "$(changed allowed "Add :$entry:Allowed bool true")"
expect_fail 'self-test: inner payload type' check_payload_type \
    "$(changed inner-type 'Set :PayloadContent:0:PayloadType com.apple.example')"
expect_fail 'self-test: outer payload type' check_payload_type \
    "$(changed outer-type 'Set :PayloadType com.apple.example')"
expect_fail 'self-test: scope User' check_scope \
    "$(changed scope 'Set :PayloadScope User')"
expect_fail 'self-test: equal payload UUIDs' check_distinct \
    "$(changed same-uuid "Set :PayloadContent:0:PayloadUUID $(pget "$profile" PayloadUUID)")"
expect_fail 'self-test: equal payload identifiers' check_distinct \
    "$(changed same-id "Set :PayloadContent:0:PayloadIdentifier $(pget "$profile" PayloadIdentifier)")"

# The code requirement, changed in the copy only; it is never printed.
req=$(pget "$profile" "$entry:CodeRequirement" || true)
expect_fail 'self-test: a code hash' check_requirement \
    "$(with_req cdhash "$req and cdhash H\"01\"")"
expect_fail 'self-test: another identifier' check_requirement \
    "$(with_req other-ident "${req/identifier \"$ident\"/identifier \"hxprobe\"}")"
expect_fail 'self-test: no Apple anchor' check_requirement \
    "$(with_req no-anchor "${req/anchor apple generic/anchor trusted}")"
expect_fail 'self-test: no team clause' check_requirement \
    "$(with_req no-team "identifier \"$ident\" and anchor apple generic")"
expect_fail 'self-test: no code requirement' check_requirement \
    "$(changed no-req "Delete :$entry:CodeRequirement")"

# The team check on a profile with a made-up team, quoted and unquoted, and
# files with and without that value.
printf 'nothing to see\n' > "$tmp/clean.txt"
printf 'a line with QSTUBTEAM7 in it\n' > "$tmp/leak.txt"
for q in '"' ''; do
    p=$(with_req "team-$q" "identifier \"$ident\" and anchor apple generic and certificate leaf[subject.OU] = ${q}QSTUBTEAM7${q}")
    t="self-test: team value (quotes '$q') found"
    if [ "$(team_of "$p")" = QSTUBTEAM7 ]; then pass "$t"; else fail "$t" 'not the made-up team'; fi
    expect_pass "self-test: team value (quotes '$q') in no other file" check_team_alone "$p" "$tmp/clean.txt"
    expect_fail "self-test: team value (quotes '$q') in another file" check_team_alone "$p" "$tmp/clean.txt" "$tmp/leak.txt"
done
expect_fail 'self-test: no team value' check_team_alone \
    "$(with_req no-team-value "identifier \"$ident\" and anchor apple generic")" "$tmp/clean.txt"

# The versions of the program, its lock file and the plugin agree.
expect_pass 'versions: Cargo.toml, Cargo.lock and plugin.json agree' \
    check_versions "$cargo_toml" "$cargo_lock" "$plugin_json"
sed 's/"version": *"[^"]*"/"version": "0.0.1"/' "$plugin_json" > "$tmp/plugin.json"
expect_fail 'self-test: another plugin version' check_versions "$cargo_toml" "$cargo_lock" "$tmp/plugin.json"
awk '$0 == "name = \"hxprobe\"" { print; getline; print "version = \"0.0.1\""; next } { print }' \
    "$cargo_lock" > "$tmp/Cargo.lock"
expect_fail 'self-test: another version in Cargo.lock' check_versions "$cargo_toml" "$tmp/Cargo.lock" "$plugin_json"

# The skill: both clients, the shared program path, the denial and the
# warning; the per-user path only under the heading that explains it.
skill="$repo/skills/outlook-local-mail/SKILL.md"
skill_phrases=(warning 'cannot read the store: permission denied' "$program" \
    'Claude Desktop' 'Claude Code')
skill_fallback='When the shared directory is not used'
expect_pass 'skill: required phrases' check_phrases "$skill" "${skill_phrases[@]}"
expect_pass 'skill: description names a warning' check_skill_description "$skill"
expect_pass 'skill: per-user path only under its heading' \
    check_lines_under "$skill" 'Application Support/hxprobe' "$skill_fallback"
grep -vF -e 'Claude Desktop' "$skill" > "$tmp/skill-no-desktop.md"
expect_fail 'self-test: skill without Claude Desktop' \
    check_phrases "$tmp/skill-no-desktop.md" "${skill_phrases[@]}"
sed '/^description:/s/warning/notice/g' "$skill" > "$tmp/skill-description.md"
expect_fail 'self-test: skill description without a warning' \
    check_skill_description "$tmp/skill-description.md"
sed '/^description:/s/^description: Use when/description: Call/' "$skill" > "$tmp/skill-use-when.md"
expect_fail 'self-test: skill description not a trigger list' \
    check_skill_description "$tmp/skill-use-when.md"
{ cat "$skill"; printf '\n## Elsewhere\n\n`~/Library/Application Support/hxprobe/bin/hxprobe`\n'; } \
    > "$tmp/skill-path.md"
expect_fail 'self-test: per-user path under another heading' \
    check_lines_under "$tmp/skill-path.md" 'Application Support/hxprobe' "$skill_fallback"
printf '# Top\n\n## %s\n\n### Sub\n\n```sh\n# comment\nApplication Support/hxprobe\n```\n' \
    "$skill_fallback" > "$tmp/nested.md"
expect_pass 'self-test: a subheading and a code comment keep the heading' \
    check_lines_under "$tmp/nested.md" 'Application Support/hxprobe' "$skill_fallback"

# The changelog has a section for the version of hxprobe/Cargo.toml on top.
changelog="$repo/CHANGELOG.md"
expect_pass 'changelog: newest section is the version, with a compare link' \
    check_changelog "$changelog" "$(cargo_version "$cargo_toml")"
sed 's/^## \[\(.*\)\] - /## [9.9.9] - /' "$changelog" > "$tmp/changelog-version.md"
expect_fail 'self-test: changelog with another newest version' \
    check_changelog "$tmp/changelog-version.md" "$(cargo_version "$cargo_toml")"
grep -v '^\[[0-9.]*\]: ' "$changelog" > "$tmp/changelog-links.md"
expect_fail 'self-test: changelog without compare links' \
    check_changelog "$tmp/changelog-links.md" "$(cargo_version "$cargo_toml")"

# The README: the warning, the shared program and the profile, both
# clients and the denial; the per-user program only under its two headings.
readme="$repo/README.md"
readme_phrases=(warning "$program" "$profile_rel" 'Claude Desktop' 'Claude Code' \
    'the refresh service cannot read the store: permission denied')
readme_headings=('When the shared directory is not used' 'Upgrading from 0.2.0 and 0.2.1')
expect_pass 'readme: required phrases' check_phrases "$readme" "${readme_phrases[@]}"
expect_pass 'readme: per-user program only under its headings' \
    check_lines_under "$readme" 'Application Support/hxprobe/bin/hxprobe' "${readme_headings[@]}"
grep -vF -e "$profile_rel" "$readme" > "$tmp/readme-no-profile.md"
expect_fail 'self-test: README without the profile' \
    check_phrases "$tmp/readme-no-profile.md" "${readme_phrases[@]}"
grep -vF -e 'Claude Code' "$readme" > "$tmp/readme-no-code.md"
expect_fail 'self-test: README without Claude Code' \
    check_phrases "$tmp/readme-no-code.md" "${readme_phrases[@]}"
{ cat "$readme"; printf '\n### Elsewhere\n\n"$HOME/Library/Application Support/hxprobe/bin/hxprobe" service --stop\n'; } \
    > "$tmp/readme-path.md"
expect_fail 'self-test: README per-user program under another heading' \
    check_lines_under "$tmp/readme-path.md" 'Application Support/hxprobe/bin/hxprobe' "${readme_headings[@]}"

# The release gate (only with --release): the checksum file lists the arm64
# program of the version in hxprobe/Cargo.toml, with a 64-digit sum.
if [ "$release" = 1 ]; then
    sums=${HXPROBE_SUMS:-$repo/release/SHA256SUMS}
    version=$(cargo_version "$cargo_toml")
    t='release: version found in hxprobe/Cargo.toml'
    if [ -n "$version" ]; then pass "$t"; else fail "$t" 'no version in [package]'; fi
    expect_pass "release: $(basename "$sums") lists hxprobe-$version-macos-arm64" \
        check_release_sums "$sums" "$version"

    sum64=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    printf '# comment\n%s  hxprobe-9.8.7-macos-arm64\n' "$sum64" > "$tmp/sums-with"
    printf '# comment\n%s  hxprobe-9.8.6-macos-arm64\n%s  hxprobe-9.8.7-macos-x86_64\n0123  hxprobe-9.8.7-macos-arm64\n' \
        "$sum64" "$sum64" > "$tmp/sums-without"
    expect_pass 'release self-test: a checksum file with the line' check_release_sums "$tmp/sums-with" 9.8.7
    expect_fail 'release self-test: a checksum file without the line' check_release_sums "$tmp/sums-without" 9.8.7
    expect_fail 'release self-test: no checksum file' check_release_sums "$tmp/missing" 9.8.7
fi

if [ "$failures" -ne 0 ]; then
    printf '%d case(s) failed\n' "$failures"
    exit 1
fi
printf 'all cases passed\n'
