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

The server does not read the live store in place. It copies the store into a
private cache directory, exports the copy (as `export` does), adds indexes, a
full-text index and event times, and answers every tool call from that
database.

### Tools

Each tool returns its result as JSON text in one MCP `text` content item. A
list is `{"total": n, "offset": o, "items": [...]}`, with `total` counted
before `limit` and `offset`. Times in results are UTC, as
`YYYY-MM-DDThh:mm:ssZ`.

| Tool | Arguments | Result |
|---|---|---|
| `store_status` | none | store path, build time, whether the store changed since the build, counts of messages, folders, people and events, build time per phase, the last build error |
| `refresh` | none | rebuilds the cache now (even when the store did not change), then returns `store_status` |
| `list_folders` | `limit`, `offset` | folders with `guid, name, parent`, the counts stored by Outlook (`total_count, unread_count`) and the counts of locally cached mail (`cached_messages, cached_unread`); most cached messages first |
| `list_messages` | `folder`, `unread_only`, `has_attachment`, `from`, `since`, `until`, `limit`, `offset` | messages with `guid, subject, sender_name, sender_address, sent, is_read, has_attachment, folder, preview`; newest first |
| `search_messages` | `query` (required), plus the `list_messages` arguments | the `list_messages` fields plus `snippet`; best matches first |
| `get_message` | `guid` (required), `format`, `max_chars` | one message: the `list_messages` fields plus `internet_message_id, importance, body, body_truncated, body_format` |
| `list_attachments` | `message_guid` (required) | attachments of a message: `filename, display_name, content_type, size, inline` (metadata only, no content) |
| `search_people` | `query` (required), `limit`, `offset` | people whose name or address contains the query: `name, address` |
| `list_events` | `since`, `until`, `include_cancelled`, `limit`, `offset` | calendar events that overlap the window: `guid, subject, start, end, all_day, cancelled, location, organizer_name, organizer_address, preview`; earliest first |

Arguments:

* `limit`: 1 to 100, default 20 (a number outside that range is clamped).
  `offset`: 0 to 100,000, default 0.
* `folder`: a folder guid, or a folder name matched case-insensitively (a name
  selects every folder with that name). A folder that does not exist is an
  error.
* `from`: part of the sender's name or address. `%` and `_` are plain
  characters.
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

A missing required argument, an unknown argument name, a value of the wrong
type or out of range (except `limit`), or an unknown message guid in
`get_message` gives a
tool result with `isError: true` and a short message. `list_attachments` with
an unknown message guid returns an empty list. An unknown tool name is a
JSON-RPC error.

### Registration

Build `hxprobe` (see Build) and register the program with its absolute path.
For Claude Code:

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
```

* `--store`: the `HxStore.hxd` to serve. Without it the server looks under
  `~/Library/Group Containers/UBF8T346G9.Office/Outlook/Outlook 15 Profiles/`
  for `Main Profile/HxStore.hxd`, then `Main Identity/HxStore.hxd`, then the
  only profile directory that holds an `HxStore.hxd`. When none is found (or
  several other profiles hold one), or the path is not a regular file, the
  server exits with status 1 and a message on stderr before it reads any
  request.
* `--cache-dir`: default `~/Library/Caches/hxprobe-mcp`.
* `--refresh-secs`: the shortest time between two builds of a changed store,
  default 900; 0 rebuilds whenever the store changed.

An unknown option or a bad value exits with status 2 and the usage text.
stdout carries only JSON-RPC messages; all logging goes to stderr.

### Cache and refresh

The cache directory holds `mail.db`, a `lock` file and, during a build, the
temporary files `store.copy-<pid>` and `mail.db.tmp-<pid>`.

* A start builds nothing. A `mail.db` from an earlier run is used at once.
  When there is none, the first tool call builds it.
* A tool call rebuilds the cache when the live store's size or modification
  time differs from the one the cache was built from and the last build or
  build attempt is at least `--refresh-secs` old. A `mail.db` that cannot be
  read, has another cache format or was built for another store is rebuilt
  at once.
* The `refresh` tool rebuilds at once, whatever the interval.
* A failed build keeps the previous `mail.db` in use and is not retried
  before `--refresh-secs` have passed; `store_status` shows the error in
  `last_error`. With no previous database the tool calls fail with that
  error.
* Two servers that share a cache directory build one after the other; when
  the store did not change in between, the second uses the database the
  first built.

A build runs inside the tool call that triggers it, so that call waits for
it. On a store of 330 MB with about 21,500 messages a build takes about 11 to
13 s (the export about 10 s, the search index about 1 s); after that, list
calls take 10 to 50 ms and searches 50 to 90 ms. Raise the client's tool
timeout if it is shorter than a build, for example `MCP_TOOL_TIMEOUT` (in
milliseconds) for Claude Code.

On that store `mail.db` is about 830 MB. During a rebuild the old database,
the new one and the store copy exist side by side: about twice the database
size plus the store size. On APFS the copy is a clone and takes no extra
space.

To delete the cache, stop the server (or the client) and remove the
directory:

```sh
rm -rf ~/Library/Caches/hxprobe-mcp
```

The next start builds it again on the first tool call.

### Privacy and safety

* **Local only.** The server opens no network connections and has no tool
  that writes, sends or deletes mail.
* **Read-only, on a private copy.** It reads the live store only to copy it
  (and its first bytes at start, to check the version) and never writes to
  it.
* **Owner-only files.** The cache directory is mode `0700` and its files
  `0600`. An existing cache directory that is a symbolic link, not a
  directory, or owned by another user is refused; a wider mode is tightened
  to `0700`.
* **The cache is a plaintext copy of the mailbox.** `mail.db` holds the
  cached messages, bodies, people and events unencrypted. Protect it like the
  mailbox itself, or delete it when it is no longer needed.
* **Logs.** stderr carries tool names, counts, timings, paths and error
  messages, never message content. A crash prints only where it happened.
* **Untrusted content.** Messages come from other people. The server tells
  the client, in its instructions and in every tool description, that message
  content is untrusted data and must not be treated as instructions. An agent
  that reads mail can still be steered by what the mail says; review what it
  does with the results.
* **macOS permission.** The store lives in another app's container. macOS may
  ask whether the MCP client app (the terminal or the AI app that starts the
  server) may read another app's data.

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

## License

MIT. Portions derived from the MIT-licensed hxprobe project by securized; see
LICENSE.
