# Changelog

All notable changes to this project are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.4] - 2026-11-24

The hxprobe program has no functional change; its version follows the plugin.

### Added

- The skill carries its version: `metadata.version` in its front matter and
  a `Skill version:` line in its body. `scripts/test-repo.sh` fails when
  either differs from the version of the program.
- The skill covers triage and threads: which mail is important, urgent or
  waiting on the user, the analysis of a whole thread, and "did I answer
  this". The steps are in `skills/outlook-local-mail/references/triage-and-threads.md`;
  the rule that a message is never called open before the sent folder and
  the newest message of its thread were checked is in the skill itself.
- The skill says how to answer "which version": `service.version` of
  `store_status` for the server, its own version line for the skill.

### Changed

- The skill reports attachments with `inline` 0 as the attached files and
  the others as embedded images; this holds for nearly all entries.
- The description of the skill is shorter (308 characters, was 567), a quoted string, and
  names triage, threads and the reply check as triggers.
  `scripts/test-repo.sh` checks the quotes and that every reference file the
  skill names exists.

### Removed

- The section "Permission errors (macOS Full Disk Access)" of the skill
  with all its parts (managed Macs, administrators, the fallback directory,
  restarting the client). On a permission error the skill now gives the user
  the whole error text unchanged, which names the fix. The README keeps the
  steps and the material for administrators. `scripts/test-repo.sh` no
  longer requires the program path, the stop command or the client names in
  the skill.

## [0.2.3] - 2026-11-24

The hxprobe program has no functional change; its version follows the plugin.

### Changed

- The skill asks whether the Mac is managed by an organisation when a
  permission error appears. On a managed Mac it no longer walks the user
  through System Settings: the user sends the error text to the
  administrator, who installs the device profile
  `mdm/hxprobe-full-disk-access.mobileconfig` through device management. The
  Full Disk Access steps stay for the user's own Mac.
- The skill and the README no longer say that Full Disk Access for the MCP
  client does not help: a grant for the app that starts the server has been
  seen to be enough on its own. The skill offers it as the next step when a
  grant for the program does not remove the error.

## [0.2.2] - 2026-11-24

### Added

- A `warning` field in the results of the seven mail tools while older data
  is served because the cache is not being refreshed:
  `mail data is from <time> and is not being refreshed: <error>`. It is
  absent when all is well; `store_status` and `refresh` do not carry it.
- `mdm/hxprobe-full-disk-access.mobileconfig`: a device profile that grants
  Full Disk Access to `/Users/Shared/hxprobe/bin/hxprobe` with the release
  signature, for organisations that manage their Macs.
- The README section "For administrators: Full Disk Access by device
  profile".
- `scripts/release-sign.sh --compare <signed program>` checks the designated
  requirement of a signed program against the profile and verifies its
  signature against the profile's requirement; signing compares the
  requirements too and stops on a difference.
- `scripts/test-repo.sh`: checks of the profile, the versions and the
  documents; `--release` also requires the checksum line of the version.

### Changed

- A store that macOS denies gives an error that names the remedy and the
  program to add, in the service form
  `the refresh service cannot read the store: permission denied. Fix: open System Settings, Privacy & Security, Full Disk Access, add "<program>" and turn it on; then call again after 10 seconds. ...`
  and in fallback mode
  `this server cannot read the store: permission denied. Fix: ...`. The
  service, the server that starts it and the fallback mode give it in the
  same words. The leading phrase is unchanged.
- A server whose store cannot even be examined at start (no access) starts
  anyway and answers with that error; after the grant the next call after
  about 10 seconds works, without a restart of the client.
- The server's MCP instructions tell the client what to do with a
  `warning`.
- The plugin installs the program at `/Users/Shared/hxprobe/bin/hxprobe`,
  the same path for every account, when that directory is the user's own or
  owned by root; otherwise it falls back to
  `~/Library/Application Support/hxprobe` and the permission errors say why.
  A root-owned program is only run when its SHA-256 is listed.
- The launcher checks the SHA-256 of the installed program at every start; a
  release program that changed after its install is not run. The install
  lock has mode `0600`.
- A server whose program is denied the store keeps an older service that
  works in use, from the first contact, instead of stopping it.
- A server started without `--store` whose lookup of the default store is
  denied starts too and answers with the permission error; when the lookup
  is allowed and finds no store, its calls return
  `no Outlook store found under the default profile directory; pass --store`.
- The stop command in the permission error has its paths in single quotes,
  for a shell: `'<program>' service --stop --cache-dir '<cache dir>'`.
- The `outlook-local-mail` skill covers Claude Desktop as well as Claude
  Code, the new error text, the `warning`, the shared path and the fallback
  path, and gives client-neutral advice on tool timeouts.

### Upgrade notes

- The new path `/Users/Shared/hxprobe/bin/hxprobe` needs Full Disk Access
  again: add it by hand, or have the administrator install the device
  profile before the update. Until then the mail tools answer from the old
  cache with a `warning` that names the path.
- The program of 0.2.0 and 0.2.1 in `~/Library/Application Support/hxprobe`
  is no longer used by the plugin; remove it, and its Full Disk Access entry,
  when no client registration names it.
- Restart the MCP clients after the update (quit a desktop app and open it
  again), so that they run the new version.

## [0.2.1] - 2026-11-24

Plugin release; the hxprobe program stays at 0.2.0.

### Changed

- The `outlook-local-mail` skill: one rule for getting new mail (poll
  `store_status` while a rebuild runs, otherwise `refresh`), separate advice
  after a timeout of `refresh` and of other calls, the plugin form of the
  tool names, the README address, and what to do after a permission fix when
  the server runs with `--no-service`.

## [0.2.0] - 2026-11-24

### Added

- `hxprobe service`: one background service per cache directory that is the
  only process rebuilding the mail cache. Every `hxprobe mcp` server on the
  machine shares it, so several MCP clients no longer build the same data
  several times. The service starts on demand and exits after 30 minutes
  without clients (`--idle-secs`).
- `hxprobe service --stop` to stop the running service.
- `store_status` reports the service state (`running`, `building`, `version`,
  `pid`, `last_error`).
- `--no-service` and `HXPROBE_SERVICE=0` keep the previous single-process
  behaviour.
- `scripts/release-sign.sh` signs a release program with a Developer ID
  certificate taken from `HXPROBE_SIGN_IDENTITY`.

### Changed

- The plugin launcher installs the program at the fixed path
  `~/Library/Application Support/hxprobe/bin/hxprobe` for every version, so a
  Full Disk Access entry is added once. Set `HXPROBE_INSTALL_DIR` to move it.
- The released program is signed with a Developer ID certificate and a fixed
  identifier. It is not notarized.
- Calls are answered from the current cache while a rebuild runs. Only the
  first build ever and `refresh` wait for a build.
- `refresh` that runs longer than its time limit reports that the rebuild may
  still be running instead of blocking.
- The launcher never replaces an installed program with an older one, checks
  the SHA-256 of a download before installing it, and backs off for six hours
  after a failed download when a usable program is installed.
- The `outlook-local-mail` skill covers the service, the fixed path and the
  new error messages.

### Removed

- Versioned program names (`bin/hxprobe-<version>`) in the plugin data
  directory. The launcher deletes them after a successful install.

### Upgrade notes

- Grant Full Disk Access to the new path once, then restart the MCP client.
  An old `hxprobe-0.1.x` entry can be removed.

## [0.1.1] - 2026-11-23

### Changed

- The `outlook-local-mail` skill tells the user how to grant hxprobe Full
  Disk Access after a permission error.

## [0.1.0] - 2026-11-23

### Added

- `hxprobe`: read-only reader and SQLite exporter for the local cache of New
  Outlook for Mac.
- `hxprobe mcp`: local, read-only MCP server with nine tools for mail, people
  and calendar.
- Claude Code plugin with the `outlook` MCP server and the
  `outlook-local-mail` skill.
- Prebuilt program for Apple Silicon Macs, verified by the launcher against
  `release/SHA256SUMS`.

[0.2.4]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/oleggohman/outlook-mcp/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/oleggohman/outlook-mcp/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/oleggohman/outlook-mcp/releases/tag/v0.1.0
