# Changelog

All notable changes to this project are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/oleggohman/outlook-mcp/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/oleggohman/outlook-mcp/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/oleggohman/outlook-mcp/releases/tag/v0.1.0
