# Changelog

All notable changes to this project are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-11-25

### Added

- The tool `get_attachment` sends the content of one attachment that Outlook
  has downloaded, by the `guid` that `list_attachments` returns, in the tool
  result itself: a text or HTML file (UTF-8, or UTF-16 with a byte order
  mark) as text, a PNG, JPEG, GIF or WebP image
  as an MCP `image` item, a Word, Excel or PowerPoint file (`.docx`,
  `.xlsx`, `.pptx`) as its text only, a PDF as its text and, in the same
  result, as an embedded `resource` with its bytes, and any other file as an
  embedded `resource`. The kind comes from the file's first bytes where the
  format has a signature; the content type of the record never overrides a
  signature.
- Arguments of `get_attachment`: `format` (`auto` or `text`, the text only
  without bytes), `max_bytes` (1 to 10,000,000, default 250,000; an image at
  most 3,750,000), `max_chars` (1 to 80,000, default 20,000) and `offset`
  (0 to 30,000,000 characters of the text, to read a long text page by page
  with `next_offset`). Bytes are sent whole or not at all; a PDF above the
  limit comes with its text and a note; no file above 30,000,000 bytes is
  read. Every result has the fields `kind`, `text`, `total_chars`,
  `truncated`, `next_offset`, `sent`, `notes`, `size`, `file_size` and
  `download_status`.
- `get_attachment` declares `_meta` `{"anthropic/maxResultSizeChars": 500000}`
  in `tools/list`, meant to let Claude Code keep results up to that size in
  the conversation (not yet checked in the clients). The default `max_bytes` is provisional until it is checked
  in the clients.
- `list_attachments` returns a `guid` per attachment.
- The hidden subcommand `hxprobe pdf-text`, which the server starts for one
  PDF to make its text (not in the usage text).
- Dependencies: `miniz_oxide` 0.8 (`MIT OR Zlib OR Apache-2.0`) for the
  entries of Word, Excel and PowerPoint files, whose archive and XML are
  read by this crate's own code; `pdf-extract` 0.12.1 (MIT) for the text of
  PDF files, used in the `pdf-text` child only. `Cargo.lock` gains 64
  packages (2 for `miniz_oxide`, 62 for `pdf-extract`), among them crates
  for encrypted PDFs and crates that only WebAssembly builds use. Every one
  of their licences is MIT, Zlib, or an expression that MIT or Apache-2.0
  satisfies, in three packages together with BSD-3-Clause or Unicode-3.0. The
  release program grows from about 3.4 MB to about 4.9 MB. The minimum Rust
  version stays 1.88.

### Changed

- `filename` of `list_attachments` is the file's own name (`report[7].pdf`)
  and no longer the whole value stored in the record with its folders. A
  name without folders is unchanged.
- The MCP instructions name the content of downloaded attachments and say
  that message and attachment content is untrusted.
- The `warning` key is carried by eight mail tools, `get_attachment`
  included.
- The skill covers attachment content: its description names attachments
  (341 characters), a new section says what arrives for each kind of file
  and what to do with it, and the argument table, the error table, "What
  the server cannot do" and "Common mistakes" follow the new tool. The
  reference file `references/triage-and-threads.md` sends questions about a
  file's content to `get_attachment` and no longer says attachments cannot
  be read. `scripts/test-repo.sh` checks the description's length and that
  it names attachments, and that no file under `skills/` holds a local path
  or a file address.
- The README no longer says attachments cannot be read. It has a section
  "Attachment content" (the attachment folder, what is sent, the result,
  notes, limits and errors) and covers the new tool in "Full Disk Access",
  "Privacy and safety", "Limits" and "Build".

### Security

- The file name an attachment record gives comes from mail, so it is
  checked before any file is opened: it must lie under `~/Files/` of the
  profile, with no empty, `.` or `..` part and no NUL byte; the attachment
  folder must be a real directory and the file a regular one, not a
  symbolic link; its real path must lie inside the folder; it is opened
  read-only without following a link and checked to be the same file.
- No result, error or log line carries the path of an attachment file, a
  `file://` address or a link to a file. The `uri` of a `resource` item is `hxprobe://attachment/<guid>`,
  an id that cannot be opened.
- PDF text is made in a child process, never inside the server: an empty
  environment, no argument but `pdf-text`, the bytes on its input, at most
  20 seconds and 16,000,000 bytes of text, and its whole process group
  killed when a limit is passed. The child's own allocator ends it with
  exit status 4 once it holds more than 1 GiB at once, so a compressed
  stream that inflates without end cannot fill the Mac's memory; the
  allocator of every other command only counts. Any failure means "no
  text" for that call. The Word, Excel and PowerPoint readers bound the
  entries, the inflated bytes and the text they produce.
- Attachment content is marked untrusted in the instructions, in the tool
  description and with the key `untrusted` in every `get_attachment` result.
- Nothing is written and nothing is copied into the cache: a file is read in
  place, read-only, at call time. No network.

### Upgrade notes

- The cache format and the service are unchanged; a server of 0.2.x and one
  of 0.3.0 can share a cache directory.
- Restart the MCP client to get the new tool.
- Attachments are read by the server itself, not by the service: after a
  Full Disk Access grant, restart the MCP client before reading attachments.

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

[0.3.0]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.4...v0.3.0
[0.2.4]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/oleggohman/outlook-mcp/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/oleggohman/outlook-mcp/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/oleggohman/outlook-mcp/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/oleggohman/outlook-mcp/releases/tag/v0.1.0
