# hxprobe

`hxprobe` is a local, read-only reader and exporter for `HxStore.hxd`, the
cache file of **New Outlook for Mac**. It verifies the store's blocks, lists
the object types it holds, prints the fields of single objects, and exports
every typed object to a SQLite database. It never writes to the store and
needs no network access.

[SPEC.md](SPEC.md) describes the file format: file header, block container,
checksums, LZ4 framing, keys and revisions, values, typed objects, the
property schema, the export database and the open questions.

## Build

```sh
cd hxprobe
cargo build --release   # needs Rust 1.88 or later
# the program is hxprobe/target/release/hxprobe
```

SQLite is compiled in (`rusqlite` with the `bundled` feature), so there is no
dependency on a system `libsqlite3`.

## Work on a copy

Outlook holds `HxStore.lock` and rewrites the file while it runs. Always work
on a copy of the store, made while Outlook is closed. The profile directory is
named `Main Profile` on some installations and `Main Identity` on others:

```sh
PROFILES=~/"Library/Group Containers/UBF8T346G9.Office/Outlook/Outlook 15 Profiles"
ls "$PROFILES"                     # Main Profile, or Main Identity
cp "$PROFILES/Main Profile/HxStore.hxd" snapshot.hxd
```

The file is memory-mapped and never read into an owned buffer, so peak memory
tracks the working set rather than the file size. The structural commands
decompress one value at a time.

## Commands

```
hxprobe blocks <file>                     verify every block, report coverage
hxprobe types  <file>                     object type histogram: id, name, count, schema, fixed_len
hxprobe dump   <file> <type> [n]          fields of n objects of a type (name or id, default 5)
hxprobe export <file> [out.db] [--force]  structural SQLite export (default hxstore.db)
hxprobe db     <file> [out.db]            legacy: SQLite database with FTS5 (default mail.db)
hxprobe map    <file> [n]                 legacy: dump the named field map for n records
hxprobe find   <file> <term> [n]          legacy: show records whose text contains <term>
hxprobe mcp    [options]                  read-only MCP server on stdin/stdout (see "MCP server")
hxprobe service [options]                 background builder of the MCP cache (see "Background service")
hxprobe service --status|--stop [--cache-dir <dir>]
                                          show or stop that builder
hxprobe --version                         print "hxprobe <version>"
```

The outputs below are made up; they show the format, not real data.

### `blocks`

```
$ hxprobe blocks snapshot.hxd
Nostromo version 'i', page size 4096
41250 block signatures found
verified 41190/41250 (99.85%), 712.4 MB decompressed
23001 blocks hold mail, 60112 IPM.Note records
  type 8   41150
  type 16  40
38420 page keys, 3 system keys, 2767 stale revisions
```

`type 8` / `type 16` are the key lengths. A few failed signatures are normal:
the magic also occurs in stale and non-block bytes.

### `types`

```
$ hxprobe types snapshot.hxd
type     name                                         count  schema        fixed_len
0x43     Root                                             1  yes           0x8a5
0x4d     View                                           310  yes           0x4d8
0x55     Person                                       20480  yes           0x15e
0xc9     MessageHeader                                33017  yes           0x60f
0xca     MessageData                                  16125  yes           0x752
0xd2     Recipient                                      300  yes           0x15a
0xd7     ?                                            22190  no            0x11a
0x1db    AddIn                                           12  size differs  0x92
...
130 types, 180212 objects
```

Counts are before deduplication. `?` means the type id has no name in the
table. `schema` is `yes` when the type has a schema and every item has the
schema's `fixed_len`, `size differs` when at least one item does not (those
items are exported generically), and `no` when the type has no schema.

### `dump`

```
$ hxprobe dump snapshot.hxd messageheader 1

=== MessageHeader (0xc9)  page 0x1c40  +2380  fixed_len 0x60f  size 3172  schema ===
  Account: ref Account:ff67b0b9ac1fdc2b1f357e54458a5302
  ...
  ConversationIndex: bytes 22
  ...
  DisplayTime: time 1718000000 2024-06-10T06:13:20Z
  ...
  HasFileAttachment: bool false
  ...
  Importance: int 1
  ...
  InternetMessageId: text "<a1b2c3d4@mail.example.com>"
  ...
  IsRead: bool true
  ...
  ItemClass: text "IPM.Note"
  ...
  MessageData: ref MessageData:3240fa455103e90a3adf3a5e5fe99589
  ...
  Preview: text "Hi Ada, the numbers for the second quarter are attached. Let me know if anything"...
  ...
  SenderDisplayName: text "Bob Sample"
  SenderEmailAddress: text "bob@example.com"
  ...
  SentTime: time 1717990000 2024-06-10T03:26:40Z
  ...
  Subject: text "Quarterly report"
  ...
  View: ref View:7c89eb8b62f12eb9a11c2fdce9ee640d
  ...

1 MessageHeader objects shown
```

The type can be a name (case-insensitive) or an id (`0xc9`, `201`). For an
item the schema applies to (`schema` in the header line), every property that
is set is listed in schema order as `name: kind value`: text cut at 80
characters, bytes as their length, times as Unix seconds and UTC, references
as `TargetType:guid`, `skipped` for a field the work budget refused. Hidden
properties appear as `ci_<n>`.

For an item without a usable schema (`generic` in the header line) each line
is a discovered field: offset in the item, `field_<offset>`, length in bytes,
how it decoded (`utf16`, `utf8`, `binary`, `ticks`, `skipped`) and the value.

### `export`

```
$ hxprobe export snapshot.hxd out.db
out.db: 38420 values, 137410 objects, 12007 duplicates, 7120 fields, 95880 collections
objects: 104230 schema (35918 superseded copies), 33180 generic; schema types on the generic path: 12 fixed_len mismatches, 5 without GUID
```

### Legacy `db`, `map`, `find`

These commands read messages without the object layer, by carving UTF-16LE
text around each `IPM.Note` record (SPEC.md §10). `db` writes a `messages`
table with an FTS5 index, `map` prints the named field map of records (`n = 0`
prints only coverage per key), and `find` prints records whose text contains a
term. This path is Latin-only by design; use `export` or `dump` for text in
other scripts.

```sh
hxprobe db snapshot.hxd mail.db
sqlite3 mail.db "SELECT sent_utc, sender, subject
                 FROM messages
                 WHERE id IN (SELECT rowid FROM messages_fts
                              WHERE messages_fts MATCH 'invoice')
                 ORDER BY sent_unix DESC LIMIT 20;"
```

## What the export contains

SPEC.md §9 has the details.

* **One table per object type with a schema**, named exactly as the type
  (`"MessageHeader"`, `"MessageData"`, `"View"`, `"Person"`,
  `"AttachmentHeader"`, ...), one row per object, one column per property.
  Every table starts with `object_id`, `guid` and `page`. References are
  stored as the target's GUID in hex, so `ON a."View" = b.guid` is a join.
  Quote table names: `View` is an SQL keyword.
* **`messages`**: one row per message header with its folder name and body:
  `guid, Subject, SenderDisplayName, SenderEmailAddress, SentTime,
  DisplayTime, IsRead, Importance, HasFileAttachment, Preview,
  InternetMessageId, folder, folder_guid, body_html, body_first,
  body_encoding`.
* **`folders`**: one row per folder (`View`) with its parent:
  `guid, Name, Type, TotalCount, UnreadCount, parent, parent_guid`.
* **`recipients`** (`object_id, page, guid, display_name, email, GivenName,
  Surname`) and **`people`** (`object_id, page, guid, name, email`).
* `message_headers`, `message_data`, `views`: all columns of the
  `MessageHeader`, `MessageData` and `View` tables.
* `objects` (one row per exported object, `has_schema` 1 or 0), `fields`
  (variable fields of objects without a usable schema) and `collections`
  (node-pack collection entries).

Times are Unix seconds. The outputs below are made up.

Unread messages per folder:

```sh
sqlite3 out.db "SELECT folder, COUNT(*) AS messages, SUM(IsRead = 0) AS unread
                FROM messages GROUP BY folder_guid ORDER BY unread DESC LIMIT 3;"
```
```
Inbox|1840|212
Newsletters|630|95
Projects|118|7
```

Latest messages with sender and subject:

```sh
sqlite3 out.db "SELECT datetime(SentTime, 'unixepoch'), SenderEmailAddress, Subject
                FROM messages ORDER BY SentTime DESC LIMIT 3;"
```
```
2024-06-10 03:26:40|bob@example.com|Quarterly report
2024-06-09 02:26:40|carol@example.com|Re: Team lunch
2024-06-07 22:40:00|ada@example.com|Draft agenda
```

Messages with a stored body (`body_html` is `MessageData.HTMLBodyBytes`, as
text when it decodes as text; NULL when the store has no body for the
message):

```sh
sqlite3 out.db "SELECT Subject, length(CAST(body_html AS BLOB))
                FROM messages WHERE body_html IS NOT NULL
                ORDER BY SentTime DESC LIMIT 3;"
```
```
Quarterly report|18630
Re: Team lunch|5214
Draft agenda|962
```

Senders with the most messages:

```sh
sqlite3 out.db "SELECT SenderEmailAddress, COUNT(*) FROM messages
                GROUP BY SenderEmailAddress ORDER BY 2 DESC LIMIT 3;"
```
```
news@example.com|412
bob@example.com|96
ada@example.com|71
```

Folder tree, each folder with its parent:

```sh
sqlite3 out.db "SELECT parent, Name, TotalCount, UnreadCount FROM folders
                WHERE parent IS NOT NULL ORDER BY parent, Name LIMIT 3;"
```
```
Inbox|Newsletters|630|95
Inbox|Projects|118|7
Projects|Archive 2023|41|0
```

Contacts with an address at one domain:

```sh
sqlite3 out.db "SELECT name, email FROM people
                WHERE email LIKE '%@example.org' ORDER BY name LIMIT 3;"
```
```
Ada Example|ada@example.org
Carol Sample|carol@example.org
Dan Test|dan@example.org
```

A type table directly: attachments per message. `AttachmentHeader` has a
`MessageHeader` reference (the table exists only when the store holds
attachments):

```sh
sqlite3 out.db 'SELECT h.Subject, COUNT(*) AS attachments
                FROM "AttachmentHeader" a
                JOIN "MessageHeader" h ON a."MessageHeader" = h.guid
                GROUP BY h.guid ORDER BY attachments DESC LIMIT 3;'
```
```
Quarterly report|4
Photos from the trip|3
Invoice 2024-06|1
```

Objects per type:

```sh
sqlite3 out.db "SELECT type_name, has_schema, COUNT(*) FROM objects
                GROUP BY type_id ORDER BY 3 DESC LIMIT 3;"
```

A few text values contain an embedded NUL character. The `sqlite3` command
line tool stops at the first NUL, in `length()` and when printing. Use
`hex(col)` or `length(CAST(col AS BLOB))`, or read the database from a client
library (Python's `sqlite3` returns the whole string).

## MCP server

`hxprobe mcp` is a local, read-only MCP server for an AI client. It speaks
MCP over stdin/stdout (newline-delimited JSON-RPC 2.0: `initialize`, `ping`,
`tools/list`, `tools/call`) and lets the client list folders, list and search
messages, read a message body, list attachments, search people and list
calendar events. It has no write tools and no SQL tool, and it opens no
network connections.

The server does not read the live store in place. The cache is built by
copying the store into a private cache directory, exporting the copy (as
`export` does) and adding indexes, a full-text index and event times. Every
tool call is answered from that database. By default the build runs in a
separate background process, `hxprobe service`, one per cache directory (see
"Background service"); the MCP servers only read the database.

### Tools

Each tool returns its result as JSON text in one MCP `text` content item. A
list is `{"total": n, "offset": o, "items": [...]}`, with `total` counted
before `limit` and `offset`. Times in results are UTC, as
`YYYY-MM-DDThh:mm:ssZ`.

| Tool | Arguments | Result |
|---|---|---|
| `store_status` | none | store path, build time, whether the store changed since the build, counts of messages, folders, people, events and objects, the fixed-length mismatch count, build time per phase, the last build error, and the background service (`service`, see "Background service"); answered at once |
| `refresh` | none | rebuilds the cache now (even when the store did not change), waits for the build, then returns `store_status` |
| `list_folders` | `limit`, `offset` | folders with `guid, name, parent`, the counts stored by Outlook (`total_count, unread_count`) and the counts of locally cached mail (`cached_messages, cached_unread`); most cached messages first |
| `list_messages` | `folder`, `unread_only`, `has_attachment`, `from`, `since`, `until`, `limit`, `offset` | messages with `guid, subject, sender_name, sender_address, sent, is_read, has_attachment, folder, preview`; newest first |
| `search_messages` | `query` (required), plus the `list_messages` arguments | the `list_messages` fields plus `snippet`; best matches first |
| `get_message` | `guid` (required), `format`, `max_chars` | one message: the `list_messages` fields plus `internet_message_id, importance, body, body_truncated, body_format` |
| `list_attachments` | `message_guid` (required) | attachments of a message: `filename, display_name, content_type, size, inline` (metadata only, no content) |
| `search_people` | `query` (required), `limit`, `offset` | people whose name or address contains the query: `name, address` |
| `list_events` | `since`, `until`, `include_cancelled`, `limit`, `offset` | calendar events that overlap the window: `guid, subject, start, end, all_day, cancelled, location, organizer_name, organizer_address, preview`; earliest first |

Arguments:

* `limit`: 1 to 100, default 20 (an integer outside that range is clamped; a
  number that is not a 64-bit integer, such as `1e30`, is an error).
  `offset`: 0 to 100,000, default 0.
* `folder`: a folder guid, or a folder name matched case-insensitively (a name
  selects every folder with that name), at most 200 characters. A folder that
  does not exist is an error.
* `from`: part of the sender's name or address, at most 200 characters. `%`
  and `_` are plain characters.
* `since` (inclusive) and `until` (exclusive): `YYYY-MM-DD` (midnight UTC),
  `YYYY-MM-DDThh:mm:ssZ`, or Unix seconds (as a string or a number). For
  `list_messages` and `search_messages` they apply to the sent time. For
  `list_events`, `since` defaults to now and `until` to `since` plus 14 days,
  and `until` must be after `since`.
* `query` of `search_messages`: 1 to 200 characters. Every word must match a
  whole word in the subject, sender, preview or body. Quotes, `*`, `AND`,
  `NEAR` and other search syntax are plain text. A query without letters or
  digits finds nothing.
* `query` of `search_people`: 1 to 100 characters, part of a name or address.
* `guid`, `message_guid`: 32 lowercase hex characters.
* `format`: `text` (default; the HTML body converted to plain text) or `html`.
  `max_chars`: 1 to 80,000, default 20,000; `body_truncated` tells whether the
  body was cut. `body` is null when the store holds no body for the message.
* `include_cancelled`: default false.
* An integer may be written with a zero fraction (`20.0`). A string argument
  must not contain control characters other than tab and line breaks.

A missing required argument, an unknown argument name, a value of the wrong
type or out of range (except `limit`), or an unknown message guid in
`get_message` gives a
tool result with `isError: true` and a short message. `list_attachments` with
an unknown message guid returns an empty list. An unknown tool name is a
JSON-RPC error.

### Install as a Claude Code plugin

The repository is also a Claude Code plugin. It provides the `outlook` MCP
server and the `outlook-local-mail` skill (`skills/`), which tells Claude Code
how to use the tools. The plugin starts the server through
`scripts/run-mcp.sh`, which finds or installs the `hxprobe` program for the
version in `hxprobe/Cargo.toml` and then runs `hxprobe mcp`.

The program is installed at one stable path for every version:

```
<install dir>/bin/hxprobe
```

`<install dir>` is `$HXPROBE_INSTALL_DIR`, or
`~/Library/Application Support/hxprobe` when that is not set. The directory
and its `bin` folder must belong to the current user; the launcher sets both
to mode `0700`. The path stays the same across upgrades, so every client and
every version use one program file and one Full Disk Access entry.

On each start the launcher asks the installed program for its version
(`hxprobe --version`):

* The version of the plugin, or a newer one: it is run as it is. The launcher
  never downgrades; with a newer program it logs that it runs it.
* Missing, older, or no answer: a new program is installed under the lock
  `<install dir>/install.lock`, by a rename over `bin/hxprobe`. A running
  server or service keeps its old file.

How a new program is installed:

* When `release/SHA256SUMS` lists a prebuilt program for the release and the
  Mac's architecture, the launcher downloads it from the GitHub release over
  HTTPS and installs it only if its SHA-256 matches the listed sum. A file
  that does not match is deleted and never run, and nothing is built in its
  place. With no program of the wanted version installed the launcher then
  stops, also when an older one is installed. With a program of that version
  built here, the launcher logs the mismatch and runs that program.
* When the download fails (no network, for example) and a program that
  answers `--version` is installed, that program is run, even an older one.
  With an older one the next start tries the download again.
* When a program of the wanted version built here is run because the
  download failed or the file did not match, the launcher leaves
  `<install dir>/bin/.download-failed` and does not try the download again
  for about 6 hours, so the starts in that time do not each wait for it. The
  next install removes the file; delete it to have the next start try again.
* Otherwise the launcher builds the program with
  `cargo build --release --locked`. This needs Rust (install it from
  <https://rustup.rs>) and takes about a minute, so the first connection can
  time out; reconnect the MCP server once the build has finished. Without a
  prebuilt program and without `cargo` the server does not start, and the
  message on stderr says what to do.
* A new program must report the wanted version or a newer one; otherwise it
  is deleted and not run.

The launcher records where the installed program came from in
`<install dir>/bin/.origin`: `release <sha256>` for a verified download,
`build` for a program built with `cargo` (no record counts as `build`). A
program built here is replaced by the release program of the same version as
soon as `release/SHA256SUMS` lists one. A program built here is not signed
with the release identity, so its Full Disk Access grant may not survive the
next build; the launcher logs a note when it builds.

To use a program you built yourself, set `HXPROBE_BIN` to its absolute path
in the environment of Claude Code. The launcher then runs it as it is and
installs nothing; the background service is started from that same program,
which needs its own Full Disk Access entry (see below).

Upgrading from 0.1.x: earlier launchers kept the program as
`bin/hxprobe-<version>` in the plugin's data directory (`$CLAUDE_PLUGIN_DATA`,
or `~/Library/Application Support/hxprobe-plugin` when that is not set). After
a successful install those `hxprobe-*` files, and the build directories and
lock that earlier launchers left there, are removed. The Full Disk Access
entry of the old `hxprobe-0.1.x` program no longer applies; add the new path
(see below) and remove the old entry. Restart the MCP clients after the
update, so that no 0.1.x server keeps building the cache on its own.

### Full Disk Access

The Outlook store lives in another app's container. Depending on how the MCP
client starts the server, macOS checks the permission of the hxprobe program
itself, not that of the client; giving the client Full Disk Access then does
not help. When a tool or `store_status` reports `Operation not permitted` or
`permission denied` for the store, give the program Full Disk Access:

1. Open System Settings, Privacy & Security, Full Disk Access and click
   **+**.
2. Press Cmd+Shift+G, paste the path of the program and click Open. The path
   is `~/Library/Application Support/hxprobe/bin/hxprobe`, or
   `$HXPROBE_INSTALL_DIR/bin/hxprobe` when that is set, or the program named
   by `HXPROBE_BIN`. Make sure its toggle is on.
3. Stop the background service, so that the next tool call starts one with
   the permission (this works while the client stays open):

   ```sh
   "$HOME/Library/Application Support/hxprobe/bin/hxprobe" service --stop
   ```

   Add `--cache-dir <dir>` when the server runs with another cache directory,
   and use the program's real path when it is elsewhere.

macOS remembers the grant by the program's code signature (Apple, TN3127
"Inside Code Signing: Requirements"). Releases from 0.2.0 on are signed with a Developer ID
and a fixed signing identifier, and an upgrade replaces the file at the same
path, so the grant is expected to survive upgrades without a new entry. A
program you built yourself (the `cargo` fallback, or `HXPROBE_BIN`) is not
signed that way, so its grant may need to be given again after each build.

### Signed, not notarized

Releases from 0.2.0 on are signed with a Developer ID but are not notarized.
The launcher's own download is not affected. A release program downloaded by
hand with a browser from the release page is blocked by macOS on its first
run; allow it once in System Settings, Privacy & Security.

### Registration

Build `hxprobe` (see Build) and register the program with its absolute path.
When the plugin is installed too, register its installed program instead
(`~/Library/Application Support/hxprobe/bin/hxprobe`), so that every client
uses one program and one Full Disk Access entry. For Claude Code:

```sh
claude mcp add outlook -- /absolute/path/to/hxprobe mcp
```

Other MCP clients take a JSON entry such as:

```json
{"mcpServers": {"outlook": {"command": "/absolute/path/to/hxprobe", "args": ["mcp"]}}}
```

Options go after `mcp`, in `args` for the JSON form.

### Options

```
hxprobe mcp [--store <file>] [--cache-dir <dir>] [--refresh-secs <n>]
            [--idle-secs <n>] [--no-service]
```

* `--store`: the `HxStore.hxd` to serve. Without it the server looks under
  `~/Library/Group Containers/UBF8T346G9.Office/Outlook/Outlook 15 Profiles/`
  for `Main Profile/HxStore.hxd`, then `Main Identity/HxStore.hxd`, then the
  only profile directory that holds an `HxStore.hxd`. When none is found (or
  several other profiles hold one), or the path is not a regular file, the
  server exits with status 1 and a message on stderr before it reads any
  request. The path is resolved at start to an absolute path without
  symbolic links; `store_status` shows that path, and the cache is kept for
  it.
* `--cache-dir`: default `~/Library/Caches/hxprobe-mcp`. The directory must
  be on a filesystem that keeps Unix permissions (not exFAT or a network
  share without them).
* `--refresh-secs`: the shortest time between two builds of a changed store,
  default 900; 0 rebuilds whenever the store changed.
* `--idle-secs`: the background service this server starts exits after this
  many seconds without a request, default 1800 (30 minutes); 0 means never.
* `--no-service`, or `HXPROBE_SERVICE=0` in the environment: fallback mode.
  The server builds the cache itself, inside the tool call that needs it, and
  starts no service (see "Cache and refresh").

`--refresh-secs` and `--idle-secs` are passed on to the service the server
starts. A running service keeps the values it was started with, also for
servers started later with other values; `service --status` shows them.

An unknown option or a bad value exits with status 2 and the usage text.
stdout carries only JSON-RPC messages; all logging goes to stderr. A closed
stderr does not stop the server.

### Background service

`hxprobe service` is the one process per cache directory that builds and
refreshes `mail.db`. The MCP servers that use the directory never build in
service mode; they tell the service that a client is active, open `mail.db`
again when the service replaced it, and answer from it.

* **Started automatically.** The first tool call that finds no service
  running starts one, from the same program file as the server, in the
  background. No launchd job is installed, and no client stops it when it
  quits.
* **One per cache directory.** Several servers, also from several MCP
  clients, share one service and one database.
* **Idle exit.** The service exits after `--idle-secs` (default 30 minutes)
  without a request; the next tool call starts it again.
* **Upgrades.** A server that finds an older service stops it and starts its
  own; a newer service is used as it is.
* **Another store.** A cache directory serves one store at a time: the store
  of the server that started the service. A server with another `--store` gets
  the error `this cache directory serves another store: <path>; use another
  --cache-dir`. After an idle exit or a stop, the first caller's store wins,
  so the roles can change. Use one `--cache-dir` per store.

Show the status of the service, or stop it:

```sh
"$HOME/Library/Application Support/hxprobe/bin/hxprobe" service --status
"$HOME/Library/Application Support/hxprobe/bin/hxprobe" service --stop
```

Add `--cache-dir <dir>` when the server runs with another cache directory
(and use the program's real path when it is not installed there).

* `--status` prints the service's status as one JSON line on stdout: `ok`,
  `version`, `pid`, `store_path`, `building`, `db_usable`, `built_unix`,
  `store_changed`, `last_attempt_unix`, `last_error`, `refresh_secs`,
  `idle_secs`, `started_unix` and `builds`. Exit status 0; 1 with
  `hxprobe service: the refresh service is not running (<dir>)` on stderr
  when no service answers.
* `--stop` asks the service to stop, whatever its version, and waits up to
  5 s until it is gone. Exit status 0 when it stopped or none was running. A
  service that does not answer or does not stop gives exit status 1 and a
  message with its pid from `service.lock` and the command to end it
  (`kill <pid>`).
* A usage error exits with status 2. The service process itself exits with
  0 (idle, stop request, signal, or its socket was removed), 1 (it cannot
  start: store, directory, socket), 2 (usage), 75 (another service holds the
  directory) or 77 (the store cannot be read: permission denied).

The service writes its log to `<cache dir>/service.log`
(`~/Library/Caches/hxprobe-mcp/service.log` by default): start, builds with
their reason, timings and counts, failed builds, and why it exited. A log
larger than 5 MB is moved to `service.log.1` when a server starts a new
service.

`store_status` has a `service` object in service mode (`null` in fallback
mode):

```json
"service": {"running": true, "building": false, "version": "0.2.0", "pid": 4242, "last_error": null}
```

`running` tells whether the service answered the last request and `building`
whether a rebuild is running. `last_error`, here and at the top level of
`store_status`, is the error of the last build, or why the service is not
available.

Errors of the tools in service mode:

| Error | Meaning and what to do |
|---|---|
| `the mail cache is being built for the first time; call again in a few seconds` | No cache exists yet and the first build took longer than the 50 s a call waits. Call again. |
| `this server is older than the mail cache; restart the MCP client` | A newer version built the cache in a format this server cannot read. Restart the MCP client, so it starts the new version. |
| `this cache directory serves another store: <path>; use another --cache-dir` | See "Another store" above. |
| `the rebuild did not finish within 300 s and may still be running; call store_status later` | `refresh` stopped waiting; the build goes on. Check `store_status` later. |
| `the refresh service did not start; see <cache dir>/service.log` | No service answered within 10 s. See the log; a new start is tried after 30 s. |
| `the refresh service exited at start (<status>); see <cache dir>/service.log` | The service failed at start. See the log. |
| `the refresh service cannot read the store: permission denied; see <cache dir>/service.log` | See "Full Disk Access". |

When a usable `mail.db` exists, a service that is not available does not
stop the tools: calls are answered from the database on disk, and the reason
shows in `last_error`.

Fallback mode (`--no-service`, `HXPROBE_SERVICE=0`) is also used when the
path of the socket `<cache dir>/service.sock` is too long for a Unix socket
address (about 100 bytes on macOS); the server logs why at start.

On Linux, a server whose program file was replaced while it runs may be
unable to start the service; the error then asks to restart the MCP client.

### Cache and refresh

The cache directory holds:

| File | Purpose |
|---|---|
| `mail.db` | the database the tools read |
| `lock` | build lock |
| `store.copy-<pid>`, `mail.db.tmp-<pid>` | temporary files of a running build |
| `service.lock` | held by the running service; holds its pid and version |
| `service.sock` | socket of the running service |
| `service.log`, `service.log.1` | log of the service, and one older log |

In service mode:

* When there is no usable `mail.db`, the service builds it right after it
  starts, and the first tool call waits for that build (up to 50 s).
* The service rebuilds when all three hold: the live store's size or
  modification time differs from the one the cache was built from; the last
  build or build attempt is at least `--refresh-secs` old (a time later than
  the current time does not count); and a tool call arrived since the last
  build started. A service without calls does not rebuild.
* The rebuild runs in the background. The tool call that found the store
  changed is answered at once from the previous database, so recent mail can
  be missing from that answer; `store_status` shows `building: true` in
  `service` while it runs. Later calls read the new database once it is in
  place.
* After an idle exit, the first call starts a new service (a fraction of a
  second) and is answered from the existing database; the rebuild that
  follows a long idle period is not waited for, so that first answer may come
  from the older snapshot.
* The `refresh` tool rebuilds at once, whatever the interval, and waits for
  the build (up to 300 s). When a build is already running, it waits for that
  one instead of starting a second one.
* A failed build keeps the previous `mail.db` in use and is not retried
  before `--refresh-secs` have passed; `store_status` shows the error in
  `last_error`. With no previous database the tool calls fail with that
  error, and a new attempt needs a tool call and waits at least
  `--refresh-secs` (and at least 60 s) after the failed one.
* A `mail.db` that cannot be read, has another cache format, was built for
  another store, or is not a regular file owned by the user and closed to
  group and others counts as missing. A `lock` that is not such a file is
  removed and created again.

In fallback mode the server builds the cache itself, inside the tool call
that needs it, with the same rules except the one about a tool call: that
call waits for the build. Servers that share a cache directory build one
after the other; a server uses a database another one built less than
`--refresh-secs` ago.

On a store of 330 MB with about 21,500 messages a build takes about 11 to
13 s (the export about 10 s, the search index about 1 s); after that, list
calls take 10 to 50 ms and searches 50 to 90 ms. Only the first call ever,
`refresh` and, in fallback mode, a call that triggers a rebuild wait for a
build. Raise the client's tool timeout if it is shorter than a build, for
example `MCP_TOOL_TIMEOUT` (in milliseconds) for Claude Code.

On that store `mail.db` is about 830 MB. During a rebuild the old database,
the new one and the store copy exist side by side: about twice the database
size plus the store size. On APFS the copy is a clone and takes no extra
space.

To delete the cache, stop the service (and the servers in fallback mode),
then remove the directory:

```sh
"$HOME/Library/Application Support/hxprobe/bin/hxprobe" service --stop
rm -rf ~/Library/Caches/hxprobe-mcp
```

The next tool call builds it again.

### Uninstall

1. Quit the MCP clients that use the server, so that no call starts the
   service again, and stop the service:

   ```sh
   "$HOME/Library/Application Support/hxprobe/bin/hxprobe" service --stop
   ```

   Add `--cache-dir <dir>` for every other cache directory in use.
2. Remove the plugin, or the MCP registration, from each client.
3. Remove the install directory and the cache directory, and the data
   directory of 0.1.x if it is still there:

   ```sh
   rm -rf ~/"Library/Application Support/hxprobe" ~/Library/Caches/hxprobe-mcp
   rm -rf ~/"Library/Application Support/hxprobe-plugin"
   ```

   Use `$HXPROBE_INSTALL_DIR`, your `--cache-dir` and `$CLAUDE_PLUGIN_DATA`
   instead when they were set.
4. Remove the `hxprobe` entry, and any old `hxprobe-0.1.x` entry, from System
   Settings, Privacy & Security, Full Disk Access.

### Privacy and safety

* **Local only.** The server and the service open no network connections and
  have no tool that writes, sends or deletes mail. Only the launcher
  downloads, and only the release program.
* **Read-only, on a private copy.** The store is read only to copy it (and
  its first bytes at server start, to check the version; the service also
  opens it at start to check the permission); nothing writes to it.
* **Owner-only files.** The cache directory is mode `0700` and its files
  `0600`. An existing cache directory that is a symbolic link, not a
  directory, or owned by another user is refused; a wider mode is tightened
  to `0700`.
* **The socket.** The service listens on the Unix socket
  `<cache dir>/service.sock` (mode `0600`), not on a network port. Only the
  user (and root) can reach it, because the directory is closed to everyone
  else; that directory mode is its access control. It carries only status,
  refresh and stop requests, never mail content.
* **The cache is a plaintext copy of the mailbox.** `mail.db` holds the
  cached messages, bodies, people and events unencrypted. Protect it like the
  mailbox itself, or delete it when it is no longer needed.
* **Logs.** The servers' stderr and `service.log` carry tool names, counts,
  timings, paths and error messages, never message content. A crash prints
  only where it happened.
* **A process without a client.** The service keeps running after the last
  client quit, until its idle time is over or it is stopped with
  `service --stop`.
* **Untrusted content.** Messages come from other people. The server tells
  the client, in its instructions and in every tool description, that message
  content is untrusted data and must not be treated as instructions. An agent
  that reads mail can still be steered by what the mail says; review what it
  does with the results.
* **macOS permission.** The program needs Full Disk Access to read the store
  (see "Full Disk Access").

### Limits

* No To/Cc recipients for a message (the recipient lists are not decoded).
* No calendar names for events.
* Recurring events appear only as stored; occurrences are not expanded.
* Only mail cached locally by Outlook; mail that exists only on the server is
  not there.
* Some messages have no stored body; `get_message` returns `body: null` for
  them.
* Enum values such as `importance` and the attachment's `inline` are numbers.
* The schema matches Outlook for Mac 16.115.1 (see Limits below).

## Safety

* **Read-only.** The store is memory-mapped read-only; no command writes to
  it. Work on a copy anyway (see above): the live file changes while Outlook
  runs.
* **Output file mode.** The `export` database holds mailbox content and is
  created with mode `0600` (owner only). The legacy `db` output is created
  with the default umask.
* **Atomic export.** `export` opens and checks the store first, writes to
  `<out.db>.tmp-<pid>` next to the output, and renames that over `out.db`
  only when the export is complete. A failed run leaves no file behind and
  does not touch an existing `out.db`.
* **Refusal rules.**
  * `export` replaces an existing `out.db` only with `--force`. Without it,
    an `out.db` that appears while the export runs is not overwritten either:
    the run fails and removes its work file. `db` always replaces its output
    file.
  * Neither command writes to an output path that is a symbolic link,
    dangling or not: name the real file instead.
  * Neither command replaces a file that is the input store, or that is
    neither empty nor a SQLite database. A swapped or mistyped argument is an
    error, not a deleted file.
  * A file without the `Nostromo` magic is refused. A version byte other than
    `i` or `h` gives a warning; parsing continues and relies on the per-block
    checksums.
  * `dump` rejects a count `n` that is not a number.
* **Work budgets.** Work per value is bounded by the size of the input, so a
  damaged or crafted file cannot make a run take hours or fill the disk. On a
  store written by Outlook none of the limits is reached; when one is,
  `export` (and `types`, `dump`, `blocks`) prints a `budget hit:` line with the
  number of items, fields or blocks left out. SPEC.md §6.8 gives the limits.

## Limits

Available through `types`, `dump` and `export`:

* Every verified block, with keys of both sizes, and one live revision per key
  (the rule "highest file offset wins" is an assumption, SPEC.md §4.2).
* Every object of every type the item envelope scan finds, not only mail:
  messages, conversations, people, recipients, folders (`View`), attachments,
  calendar items and more.
* For the object types with a schema, every property by name and type: text
  in any script, integers, flags (`IsRead`, `HasFileAttachment`, ...), times
  as Unix seconds, GUIDs, and references to other objects (message to folder,
  message to body, folder to parent folder, attachment to message).
* One row per object, identified by its GUID, although the store keeps
  several copies of most objects (which copy is current is an assumption,
  SPEC.md §6.4).

Not available:

* Mail that exists only on the server. The store is a cache, not an archive:
  for many messages it holds only a preview of about 255 characters, and full
  RFC 822 headers are never stored.
* Names and properties for object types without a schema. They are exported
  generically: variable fields as `fields` rows with their offset, no names.
  The same applies to items whose size differs from the schema.
* Names of hidden properties; they are exported as `ci_<n>`.
* The meaning of enum values (`Importance`, `FlagState`, `View.Type`,
  `BodyEncoding`, ...); they are exported as numbers.
* Links from recipients to their messages: the To/Cc/Bcc references of
  `MessageData` point to collections whose member lists are not decoded.
* Some `MessageData` items have a body descriptor that does not fit the heap;
  their body is exported as NULL.
* Folder counts: the stored `TotalCount` and `UnreadCount` of most folders do
  not match the exported messages.

The compiled schema matches Outlook for Mac **16.115.1**. On another build,
`hxprobe types` shows `size differs` for every type whose items do not match
the schema, and those items are exported generically. A type whose fields
moved without a size change would be decoded wrongly; no other build has been
tested. Windows stores (version byte `'h'`) are accepted but untested.

The tool has been tested on one store of about 330 MB. SPEC.md §12 lists the
open questions.

## Tests

```sh
cd hxprobe
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Unit and CLI tests build synthetic stores in code; no mail data is in the
repository.

The service tests (`tests/service.rs`, and the socket and process tests in
`src/mcp`) bind Unix sockets and start background processes in temporary
directories only, and take a lock so that they run one at a time. A sandbox
that forbids sockets makes them fail with `Operation not permitted`; run them
outside it.

The launcher and the release script have shell tests with stub programs on
`PATH`, no network and no signing identity:

```sh
bash scripts/test-run-mcp.sh
bash scripts/test-release-sign.sh
```

The integration test against a real store is opt-in. Without the
`HXSTORE_SNAPSHOT` variable it prints `skipped: HXSTORE_SNAPSHOT not set` and
passes:

```sh
cd hxprobe
HXSTORE_SNAPSHOT=/path/to/snapshot.hxd cargo test --release --test snapshot -- --nocapture
```

It checks aggregate facts (block verification rate, key sizes, value
classification, schema agreement of `fixed_len`, email and text ratios, one
`MessageHeader` row per GUID, message to folder and message to body links,
the `IsRead` check against a folder's stored counts, an export to a temporary
file) and prints only counts and ratios, never field contents. The temporary
database is deleted at the end. It only reads the store; point it at a copy.

## Regenerating the tables

The type name table and the property schema are committed as JSON in
`schema/` and compiled into `hxprobe` as generated Rust. After changing the
JSON or a generator, regenerate both files from the repository root and
format them:

```sh
python3 tools/gen_names.py                                         # schema/types.json -> hxprobe/src/names.rs
python3 tools/gen_props.py schema/layout.json hxprobe/src/props_gen.rs
(cd hxprobe && cargo fmt)
```

Do not edit `names.rs` or `props_gen.rs` by hand.

## Releases (maintainers)

Build the release program for each architecture from the tagged commit
(`cargo build --release --locked`), then sign it on a Mac that has the
Developer ID Application identity:

```sh
HXPROBE_SIGN_IDENTITY=<identity> scripts/release-sign.sh <program> <version> <arm64|x86_64>
```

The script writes the signed copy `hxprobe-<version>-macos-<arch>` to the
current directory and leaves the input file unchanged. It signs with the
hardened runtime, a secure timestamp and the fixed signing identifier, then
checks the signature (strict verification, a designated requirement with the
identifier, the Apple anchor and the team and without a code hash, a
Developer ID Application certificate, the runtime flag), the architecture
and, on a machine of that architecture, the version. `HXPROBE_SIGN_IDENTITY`
is required and never printed. What `codesign` prints when it fails is not
shown; run with `HXPROBE_SIGN_DEBUG=1` to see it, with the signer, team and
home directory and user names removed.

On success the script prints one line on stdout, `<sha256>  <asset>`,
computed from the signed file. Publish exactly that file as the asset of the
GitHub release `v<version>` and add the line to `release/SHA256SUMS`.

## License

MIT. Portions derived from the MIT-licensed hxprobe project by securized; see
LICENSE.
