# HxStore.hxd file format

Format of the message store used by **New Outlook for Mac** (`Nostromo`,
version `i`), as read by `hxprobe`.

Claims carry one of two labels:

* **Verified**: checked on the reference store.
* **Inferred**: consistent with the data, not independently confirmed.

The reference store is one store of 331 MB written by Outlook for Mac
16.115.1. Counts quoted from it are there to support a format claim; another
store will have different counts.

---

## 1. Overview

`HxStore.hxd` is a **paged, append-oriented key-value block store**. It is not
an ESE/JET database, not MAPI, and not encrypted. Message content is held in
independently compressed, CRC-protected blocks. Full RFC 822 headers are not
stored; sender, recipients, subject and Message-ID are stored as separate
fields.

The layers, from the file down:

1. **File header**: magic, version, page size, region offsets (§2).
2. **Blocks**: a 32-byte header (two CRCs, a magic, lengths), a key of 8 or 16
   bytes, then an LZ4-compressed value (§3).
3. **Keys**: each key can have several blocks on disk (revisions); one of them
   is live (§4).
4. **Values** under 8-byte keys are node packs or collection pages (§5).
5. **Objects** inside values: typed structs with a 0x78-byte header, fixed
   fields, two bitmaps and a heap of variable-length fields in two areas
   (§6). A per-type property schema gives the position and type of every
   field (§7).

Location (the profile directory name varies; both forms occur):

```
~/Library/Group Containers/UBF8T346G9.Office/Outlook/
  Outlook 15 Profiles/Main Profile/HxStore.hxd
  Outlook 15 Profiles/Main Identity/HxStore.hxd
```

The file **changes while Outlook runs**, and Outlook holds `HxStore.lock`.
Always work on a copy.

---

## 2. File header

All integers little-endian.

| File offset | Type | Reference value | Meaning |
|---|---|---|---|
| `+0x00` | char[8] | `Nostromo` | magic |
| `+0x08` | u64 | `0x69` (`'i'`) | format version |
| `+0x10` | u64 | `0x2798600` | live data size (**Inferred**) |
| `+0x18` | u64 | `0x5000` | directory / region size (**Inferred**) |
| `+0x20` | u64 | `0xc5000` | start of block area (**Inferred**) |
| `+0x28` | u64 | `0x12c0` | not decoded |
| `+0x30` | u64 | `0xcb20019a` | checksum-like, not decoded |
| `+0x38` | u64 | `0x1000` | **page size (4096)** |
| `+0x40` | u64 | `0x3000` | not decoded |
| `+0x48` | u64 | `0xfff1` | not decoded |
| `+0x50` | u64 | `0xdeadbeef` | guard |
| `+0x58` | u64 | `0x25000` | not decoded |
| `+0x9c` | u8[0x24] | `ef be ad de` x 9 | guard pattern |
| `+0x3fb` | u8 | `3` | not decoded |
| `+0x468` | u64 | `0x2000000` | 32 MiB cap (**Inferred**) |

**Verified**: the magic, the version byte, the page size and the guard
pattern at `+0x9c`. Every size field is a multiple of the 4096-byte page size.
The region between `0x5000` and the first block is not decoded (§12).

### Version bytes

The known magics are `NostromoH`, `NostromoH9` and `NostromoI`. The Mac build
writes `'i'` (`0x69`); Windows Mail stores use `'h'`. Treat the version byte as
a compatibility gate rather than a revision counter: accept what is known,
warn on anything else, and rely on the per-block checksums (§3.2) to catch a
real format change. `hxprobe` accepts `'i'` and `'h'`; `'h'` has **not** been
tested against a Windows store.

---

## 3. Block container

Every block is **self-validating**: two checksums and an exact inflated
length. A wrong offset fails instead of producing plausible data.

### 3.1 Header (32 bytes), key, value

```
+0x00          u32  crc32(block[0x04 .. 0x20])                     header checksum
+0x04          u32  crc32(block[0x08 .. 0x20 + key_len + val_len]) payload checksum
+0x08          u64  magic  0x5d0245643b706a05
+0x10          u32  key_len         8 or 16
+0x14          u32  val_len         compressed bytes, starting after the key
+0x18          u32  inflated_len    exact decompressed size
+0x1c          u32  codec           4 = LZ4
+0x20          key  key_len bytes
+0x20+key_len  ...  LZ4-compressed value, val_len bytes
```

Total block size on disk: `0x20 + key_len + val_len`. The magic is stored
little-endian, so on disk it reads `05 6a 70 3b 64 45 02 5d`.

**Verified**: with `+0x10` read as the key length, 77,454 blocks verify, of
which 77,385 have 8-byte keys and 69 have 16-byte keys. Every verified block
starts on a 512-byte boundary (77,454 of 77,454). `hxprobe` relies on this: a
signature that is not on a 512-byte boundary is ignored, so a block image
carried inside another block's bytes is never read as a block of its own.
`blocks` prints the number of ignored signatures when there are any.

The name `codec` and the reading of `4` as LZ4 are **Inferred**: every value
decompresses as LZ4 to its exact `inflated_len`, and `hxprobe` rejects any
other codec value.

### 3.2 Checksums

Standard **CRC-32 (IEEE, zlib polynomial `0xEDB88320`)**. Note the asymmetry:

* the **header** CRC covers `[0x04, 0x20)`, 28 bytes, excluding itself;
* the **payload** CRC starts at `0x08` (from the magic, *not* from the
  payload) and runs to the end of the block, key included.

**Verified**: the 8-byte magic occurs 77,570 times; 77,454 of those pass both
checksums and the exact inflated length. The other 116 are stale copies or
bytes that contain the magic but are not a block.

### 3.3 Locating blocks

No index of keys is decoded (§12). Blocks are found by scanning for the 8-byte
magic and subtracting 8. This is safe because of the two checksums: a false
positive does not survive validation.

### 3.4 Payload codec

LZ4 block format (no frame header, no trailing checksum), standard sequence
layout:

```
[token] [literal-length varint] [literals] [dist_lo] [dist_hi] [match-length varint]

literal_count = token >> 4         ; 15 -> add 255-continuation varint
match_length  = (token & 0x0F) + 4 ; 15 -> add 255-continuation varint
distance      = u16 little-endian, counted back from the output end
```

Minimum match is 4. Overlapping copies are legal and common (run-length
expansion), so matches must be copied one byte at a time. The final sequence
of a block is literals only.

`inflated_len` is authoritative: a correct decode lands on it exactly. Any
other outcome (short output, a distance pointing before the window, leftover
input) is a failed block, not partial output. LZ4 has no checksum of its own,
so a wrong start decodes silently into plausible garbage; the container CRCs
are what make the scan safe.

---

## 4. Keys and revisions

### 4.1 Two key sizes

Every block is one key-value pair.

* **8-byte keys** are read as a little-endian `u64`, called a **page-group
  id** here (the name is **Inferred**; the same `u64` appears in object
  references, §5.1). They hold the store's objects.
* **16-byte keys** are GUID-like byte strings, called **system records** here
  (**Inferred**). Their values are not decoded.

**Verified**: the reference store holds 73,699 distinct page-group ids and 3
distinct system keys.

### 4.2 Revisions and the live-revision rule

The store is append-oriented: rewriting a key writes a new block, and the old
one stays on disk until its space is reused. **Verified**: 3,752 verified
blocks repeat a key that already has another verified block
(77,454 blocks - 73,699 - 3 distinct keys).

Which copy is current is **not verified**. `hxprobe` assumes the block at the
highest file offset is the live one. This rule is isolated in one function
(`kv::pick_live`) so that it can be replaced once the store's index is
decoded. Everything above the key layer (`types`, `dump`, `export`) reads only
the live copy of each key.

---

## 5. Values under 8-byte keys

All integers little-endian.

### 5.1 Object reference

A 12-byte reference to an object, used by collection pages and collection
entries:

```
+0x00  u16  prefix     meaning unknown
+0x02  u64  page       page-group id
+0x0a  u16  type_id    object type (§7.6)
```

### 5.2 Node pack

A **node pack** is a value made of two length-prefixed sections:

```
u32 count_a | u32 size_a | section A (size_a bytes)
u32 count_b | u32 size_b | section B (size_b bytes)

8 + size_a + 8 + size_b == value length
```

Section A holds object items (§6); they are found by the envelope scan.
Section B holds `count_b` **collection entries** back to back, walked by
`entry_size`:

```
+0x00  u16  version          always 6
+0x02  u16                   0x570 on the reference store; not checked
+0x04  u64  page             page-group id of the collection
+0x0c  u16  collection_type  object type of the collection (§7.6)
+0x0e  u32  entry_size       bytes, this header included (>= 0x1e)
+0x12  12   owner            object reference (§5.1) to the owning object
+0x1e  ...  rest of the entry, not decoded
```

A value is a node pack only when the sizes fit exactly, every entry has
version 6 and `entry_size >= 0x1e`, and the walk of `count_b` entries ends
exactly at the end of section B.

**Verified**: 66,125 live values have the two-section shape, and the walk of
section B fits exactly on 66,124 of them. The remaining one has
`entry_size = 0xFFFFFFFE` in its first entry, probably a sentinel
(**Inferred**); it is classified as "other" and not decoded.

The names "node pack", "section A/B" and "collection entry" are this
document's.

### 5.3 Collection page

A **collection page** is a value that starts with an object reference to
itself:

```
+0x00  u32  1
+0x04  12   object reference: page == this value's own key, type_id == 0x37
+0x10  u32  member_count
+0x14  ...  index entries, layout not decoded
```

**Verified**: 7,574 live values match. With the node packs,
66,124 + 7,574 + 1 unclassified = 73,699, every live value under an 8-byte
key. Type `0x37` has no name in the type table.

The objects of the unnamed types `0xd7`, `0x105` and `0xf7` are found among
the members of collection pages; they are probably index or sort-key records
(**Inferred**).

---

## 6. Objects

### 6.1 Item envelope

An **item** is one serialized object. Its **item envelope** is the 4-byte size
prefix and the 12-byte header that open it:

```
u32 size                     prefix, not part of the item

item (offsets relative to its first byte, the 05 00):
+0   u16  5
+2   u16  fixed_len          includes this 12-byte header
+4   u32  size               equal to the prefix; counts bytes from item start
+8   u16  0
+10  u16  object_type        §7.6
item[0 .. fixed_len]         fixed part
item[fixed_len .. size]      heap
```

The **heap** is the variable-length area after the fixed part; it holds the
bytes of the item's variable fields, in two areas, heap A and heap B (§6.3).

The envelope is self-checking (the size appears twice, plus the `05 00` and
the `00 00`). **Verified**: a scan for it over every live value finds 353,931
items of 134 object types, and every item of one type has the same
`fixed_len`.

`hxprobe` scans each live value byte by byte for the envelope and accepts a
match when `16 <= fixed_len <= size` and the item fits in the value. After a
match it continues 16 bytes later rather than after the item, because items
can be nested inside another item's heap.

**Inferred**: items are also stored in structured lists (`u8 flag, u32
count`, then per item `u16 prefix, u32 size, item`). That layout is only
partly understood and `hxprobe` does not use it.

### 6.2 Item layout

The fixed part of an item has four parts:

```
item offset
+0x00        12-byte envelope (§6.1)
+0x0c        [u8; 16] GUID          the object's identity (§6.4)
+0x68        u32 A_len              size of heap A (§6.3)
             (the other header bytes up to 0x78 are not decoded)
+0x78        fields                 fixed-size properties and descriptors (§7)
fields_end   bitmap A               ceil(ci_count / 8) bytes: boolean properties
             bitmap B               ceil(ci_count / 8) bytes: meaning unknown
fixed_len    heap A, then heap B    (§6.3)
```

`fields_end` and `ci_count` (the number of properties of the type, hidden
ones included) come from the schema (§7), so

```
fixed_len = fields_end + 2 * ceil(ci_count / 8)
```

**Verified**: the formula gives the observed `fixed_len` for 92 of the 96
types with a schema that occur in the store (268,261 items). For four small
types (32 items) the observed `fixed_len` is 0x10 larger (§12). Bitmap B is
all zero in `MessageHeader`; it is not decoded.

### 6.3 Descriptors and the two heaps

A **descriptor** is an 8-byte entry in the fixed part that names a
variable-length field:

```
u32 offset     relative to the start of heap A or of heap B
u32 v          len = v & 0x7fffffff; bit 31 selects the heap
```

The heap after the fixed part is two areas:

```
A_len  = u32 at item offset 0x68 (§6.2)
heap A = item[fixed_len .. fixed_len + A_len]
heap B = item[fixed_len + A_len .. size]
```

* `len == 0` means the field is not set.
* Bit 31 **clear**: the data is `heapA[offset .. offset + len]`.
* Bit 31 **set**: the data is `heapB[offset .. offset + len]`.

Bit 31 selects the heap; it does not mean "the field is set". Reading every
field from the start of one heap only gives correct values for objects whose
heap A is empty (for example `Recipient` and `Person`).

**Verified**, on all 268,261 items whose type has a schema with their
`fixed_len`, using the schema's descriptor positions:

* every descriptor with bit 31 clear fits inside heap A;
* the descriptors with bit 31 set fit inside heap B in 267,770 items
  (99.8 %); the 491 others are all `MessageData` and are not understood (§12);
* `A_len` equals the sum of the lengths of the descriptors with bit 31 clear
  in 265,520 items;
* 187,081 items have at least one field in heap A.

With this rule, `MessageHeader.ItemClass` starts with `IPM.` in 21,532 of
21,534 objects, `SenderEmailAddress` contains exactly one `@` in 21,527, and
`InternetMessageId` starts with `<` in all of them.

`hxprobe` reads a descriptor only at the position the schema gives (§7). A
descriptor whose range lies outside its heap decodes as missing.

**Types without a usable schema** (generic path): `hxprobe` finds descriptors
by scanning the fixed part from offset 16, step 8 after a hit, else step 1. It
accepts only descriptors with bit 31 set whose range lies inside heap B:
without a schema, a descriptor with bit 31 clear cannot be told apart from
other data. So the heap A fields of these items are not exported. A scalar
field can pass the test by chance, so some generic hits may be false
positives (**Inferred**).

### 6.4 Identity, references and copies

**Own GUID.** The 16 bytes at item offset 12 identify the object.
**Verified**: of the 268,261 schema-typed items with the right `fixed_len`, 5
have an all-zero GUID (singletons such as `Root`); they have no identity and
take the generic path. The others are 199,722 distinct objects.

**Reference.** A property of size 48 (data type `0x0a`) refers to another
object:

```
+0x00  u16       version (name Inferred)
+0x02  u16       type_id of the target
+0x04  [u8; 16]  GUID of the target; all zero = no reference
+0x14  28 bytes  not decoded
```

This is not the 12-byte page reference of §5.1. Several reference properties
of one type can share one item offset (for example in `Root`, and in `View`
the `Conversations...` properties at `0xa8`); each is decoded the same way.

**Verified**: every `MessageHeader` object has a `View` reference whose GUID
is a `View` object in the store, and 99.2 % of the `MessageData` references
of `MessageHeader` name a `MessageData` object in the store.

**Copies.** The same object is stored in several pages; it is copied into each
collection that lists it (**Inferred** from where the copies sit).
**Verified**: 64,467 `MessageHeader` items are 21,534 objects.

**Which copy is current** is **not verified**. `hxprobe` assumes: the copy in
the live block with the highest file offset wins; within one block, the copy
at the higher item offset. The rule is isolated in one function
(`objects::newer`) so it can be replaced. The other copies are counted as
`superseded` and not exported.

### 6.5 Duplicates and counts

`hxprobe export` handles the copies of an object in one of two ways:

* **Schema objects** (the type has a schema, the item has the schema's
  `fixed_len`, and its GUID is not all zero): one row per `(type, GUID)`; the
  copy kept is chosen by the rule in §6.4.
* **Every other item** (generic path): deduplicated by content. The key is the
  64-bit FNV-1a hash of the item bytes together with the item size and type
  id. The row kept is the first occurrence in page-key order, so
  `objects.page` does not list every page that holds the item.

**Verified**: no two different items share a hash, size and type. Of the 134
object types seen, 96 have a name and a schema; 38 have neither and hold 24 %
of the items, most of them of types `0xd7`, `0x105` and `0xf7`.

### 6.6 Timestamps

Times are `u64` **.NET ticks**: 100-nanosecond units since
`0001-01-01T00:00:00Z`. This is *not* Windows `FILETIME` (epoch 1601).

```
unix = (ticks - 621355968000000000) / 10^7
```

For example `639201014590000000` is `2026-07-19T23:44:19Z`.

In schema objects, times are the 8-byte properties with data type 6 (§7.2).
`hxprobe` accepts a tick value when it falls between 2000-01-01 and five years
after the current date (system clock), and reports it as Unix seconds; a value
outside that window is exported as NULL. **Verified**: `SentTime` is set in
every `MessageHeader` object.

`AppointmentHeader.TimeRangeUtc` is a 16-byte property, so `export` writes it
like a GUID, as 32 hex characters (§7.2, §9). Its bytes are two little-endian
`u64` tick values: the start of the appointment, then its end. `hxprobe mcp`
reads event times from it. **Verified**: 1,233 of 1,233 `AppointmentHeader`
objects on the reference store give a start between 2021 and 2027 and an end
at or after the start.

`dump` of an item without a usable schema checks every offset from 16 to
`fixed_len - 8` against the window, which can produce false hits
(**Inferred**). The generic rows of `export` hold no times.

### 6.7 Text in fields

A property with data type 7 is a UTF-16LE string: `hxprobe` decodes it as
UTF-16LE (invalid units replaced, trailing NULs removed). Other variable
fields, and every field found on the generic path, carry no encoding marker.
`hxprobe` decodes them in this order (**Inferred** rules, checked against the
reference store):

1. **UTF-8**: drop trailing `00` bytes; accept when the rest is non-empty,
   contains no `00`, is valid UTF-8 and at least 90 % of its characters are
   printable. Tried first, because even-length ASCII or HTML is also valid
   UTF-16 and would decode as CJK.
2. **UTF-16LE**: the length must be even; drop trailing `00 00` pairs; accept
   when the rest is valid and at least 90 % of characters are not control
   characters (except tab, CR, LF), private use, noncharacters or U+FFFD.
3. Otherwise **binary**: kept as bytes.

Known limitation: a short UTF-16 string whose bytes are all printable ASCII
(for example U+4E4E, bytes `4E 4E`) is read as UTF-8.

This decodes text in any script (Latin, Hebrew, Cyrillic, CJK, emoji). It is
used by `dump` and `export`; the legacy commands (§10) do not use it.

**Embedded NULs** (**Verified**): 315 of the 338,977 text values in the
`MessageHeader` table contain U+0000 inside the text, in three columns:
`ConversationIndex` (typed as bytes in the schema and decoded as text by the
rules above), the hidden `ci_112` and `TruncatedMentions`. No text value in
the `MessageData` table contains one. `hxprobe` keeps the whole value.

### 6.8 Work budgets

The envelope scan accepts overlapping items and a descriptor may name any heap
range, so a file built for the purpose (its checksums can be made valid) could
otherwise make one value cost quadratic time or produce an export thousands of
times its size. `hxprobe` therefore bounds the work by the size of the input.
The limits sit well above what the reference store needs, and a limit that is
reached is reported (`budget hit: ...`), never applied silently.

| Budget | Limit | **Verified** on the reference store |
|---|---|---|
| Item bytes accepted per value | 4 x the value length | largest 0.9998 x; items never nest or overlap (depth 1) |
| Variable-field bytes per item | 4 x the item's heap length (heap A and B together) | no field refused (`skipped_fields` 0) |
| Body-checksum bytes per file | 4 x the file length | 0.88 x |

An envelope, field or block that does not fit the remaining budget is left out
and counted; smaller ones after it are still taken. The field budget applies
to schema properties and to generic fields alike; a schema property it refuses
is exported as NULL (`dump` shows `skipped`). The checksum budget applies to
`blocks` and to everything built on the key-value layer (`types`, `dump`,
`export`); the legacy commands verify every block they find.

---

## 7. Property schema

The schema lists, for each of 219 object types, every property with its
change index `ci`, property id, public name (or none), offset, size, data type
and child type: 4,704 properties, 3,188 of them with a public name. It matches
Outlook for Mac 16.115.1. It is committed as `schema/layout.json` and
compiled into `hxprobe` as `src/props_gen.rs` by `tools/gen_props.py`.

The schema is applied to an item only when the item's `fixed_len` equals the
schema's. Otherwise the item takes the generic path and is counted
(`fixed_len_mismatch`); `hxprobe types` shows such a type as `size differs`.

### 7.1 `schema/layout.json`

```
{"source": {"outlook": "16.115.1"},
 "types": [
  {"id": 67, "name": "Root", "fields_end": 2185, "ci_count": 108,
   "fixed_len": 2213,
   "props": [[ci, pid, name, offset, size, dt, child_type], ...],
   "named_without_row": [...]},
  ...]}
```

* `types` is strictly sorted by `id`.
* `name` is `""` for a hidden property.
* `offset` is an item offset (the 0x78-byte header included); `-1` marks a
  boolean, stored as bit `ci` of bitmap A at `fields_end` (§7.3).
* `dt` is the property's data type (§7.2); `child_type` is the object type a
  reference or collection points to, `0` when none.
* `named_without_row` lists public property names whose position is not known
  (§12).

### 7.2 Positions and data types

Positions are **Verified** by the checks in §7.4.

* `ci` is the property's position in the type's alphabetical property list,
  hidden properties included.
* A non-boolean property is at its item offset.
* A boolean property has no offset; it is bit `ci` of bitmap A (§7.3).

| size | meaning | dt values seen |
|---|---|---|
| none (boolean) | bit in bitmap A | 3 |
| 0 | variable field: an 8-byte descriptor (§6.3) | 7 = UTF-16LE string; 9, 0x16, 0x0f = bytes or arrays |
| 1 | u8 | 0x10 |
| 2 | u16 | rare |
| 4 | u32 (dt 1) or i32 (dt 2) | 1, 2, 0x13 |
| 8 | dt 6 = .NET ticks; dt 0x15 = f64; dt 4, 5, 0x0c = integer | 4, 5, 6, 0x0c, 0x15 |
| 16 | GUID | 0x0b, 0x0d, 0x17 |
| 48 | object reference (§6.4) | 0x0a |

The schema's `dt` is authoritative. A few variable fields that hold text-like
values are typed as bytes (dt 9), for example `MessageHeader.ConversationIndex`
and `Recipient.ServerId`; `hxprobe` tries the text rules of §6.7 on every
variable field that is not dt 7, and keeps bytes when they do not match.

1,516 properties have no public name. They are exported as `ci_<n>` (their
change index); a hidden property with a known property id would be named
`prop_<id>`, but the schema has none.

### 7.3 Booleans

```
value = (item[fields_end + ci / 8] >> (ci % 8)) & 1
```

**Verified** through `IsRead` (§7.4).

### 7.4 Checked rows

**Verified**, item offsets:

| Type | Id | fixed_len | fields_end, ci_count | Properties |
|---|---|---|---|---|
| MessageHeader | 0xc9 | 0x60f | 0x5e1, 178 | strings: Subject 0x598, Preview 0x518, NormalizedSubject 0x4ec, SenderDisplayName 0x56c, SenderEmailAddress 0x574, InternetMessageId 0x4cc, ItemClass 0x4d4; bytes: ConversationIndex 0x498; ticks: SentTime 0x2d8, DisplayTime 0x120, LastModifiedTime 0x178, SortTime 0x340; Importance 0x4b8 (u32), Size 0x594 (i32), FlagState 0x5cc (u8); references: View 0x380, MessageData 0x1e8; booleans: IsRead ci 73, HasFileAttachment ci 46, IsDraft ci 64 |
| View | 0x4d | 0x4d8 | 0x4b4, 139 | Name 0x448 (string), Type 0x4a8 (i32), TotalCount 0x4a0, UnreadCount 0x4ac (u32), references ParentView 0x320, Account 0x78 |
| Person | 0x55 | 0x15e | 0x158, 20 | DisplayName 0x124, EmailAddress 0x12c (strings) |
| MessageData | 0xca | 0x752 | 0x730, 132 | HTMLBodyBytes 0x67c, HTMLFirstBodyBytes 0x684 (bytes, dt 0x0f), IpmClassName 0x68c, BodyEncoding 0x610 (i32), reference ToRecipients 0x570 |
| Recipient | 0xd2 | 0x15a | 0x154, 24 | DisplayName 0x100, EmailAddress 0x108, GivenName 0x114, ReferenceId 0x134, Surname 0x14c (strings), ServerId 0x140 (bytes, dt 9), EmailType 0x110, SubType 0x148 (u32) |

Content checks on the export (**Verified**): `Recipient.EmailAddress` and
`Person.EmailAddress` look like an email address in all but a handful of
objects; `MessageHeader.Subject` and `SenderEmailAddress` are set in more than
99.9 % of rows.

**IsRead check.** For one folder (`View`) of the reference store, the number
of exported messages equals its `TotalCount` and the number with
`IsRead = 0` equals its `UnreadCount`. For most other folders the counts
differ (§12).

### 7.5 Bodies

`MessageData.HTMLBodyBytes` (0x67c) and `HTMLFirstBodyBytes` (0x684) hold the
message body; both are typed as bytes (dt 0x0f) and decoded with the text
rules of §6.7. `BodyEncoding` (0x610, i32) is exported as a number.

**Verified**: every `HTMLBodyBytes` value decodes as text, and 97 % of
`MessageData` objects have one longer than 1,000 bytes. `BodyEncoding` is
65001 in almost every object and -1 in the rest. (65001 is the Windows code
page number of UTF-8; that this is its meaning here is **Inferred**.) The 491
`MessageData` items whose `HTMLBodyBytes` descriptor does not fit heap B
(§6.3) decode the body as missing (NULL in the export when that copy is the
one kept).

How much body text a store holds varies: Outlook caches full bodies for some
messages and only a preview of about 255 characters for others, fetching the
rest from the server on demand.

### 7.6 Object type names

Object type ids run from 0 to 863; 449 of them have a name, including id 0,
`None`. The table is `schema/types.json` (type id as a decimal string mapped
to a name or `null`) and is compiled into `hxprobe` as `src/names.rs` by
`tools/gen_names.py`.

---

## 8. Minimal parser

Container and objects (what `hxprobe export` does):

```
1.  Verify file[0..8] == "Nostromo".
2.  Read page size at file+0x38 (4096).
3.  Scan for the 8-byte block magic 05 6a 70 3b 64 45 02 5d; block = hit - 8.
    Keep only blocks on a 512-byte boundary.
4.  For each block:
      a. crc32(block[0x04..0x20])  ==  u32 at block+0x00   else skip
      b. key_len  = u32 at block+0x10   (8 or 16, else skip)
         val_len  = u32 at block+0x14
         inflated = u32 at block+0x18
         codec    = u32 at block+0x1c   (4, else skip)
      c. crc32(block[0x08 .. 0x20+key_len+val_len]) == u32 at block+0x04
                                                             else skip
      d. key = block[0x20 .. 0x20+key_len]
      e. LZ4-decompress block[0x20+key_len .. 0x20+key_len+val_len];
         require exactly `inflated` bytes out, else skip.
5.  Group blocks by key; keep one live block per key (assumed: highest
    file offset).
6.  In each live value under an 8-byte key, scan for item envelopes (§6.1):
    u32 size, 05 00, u16 fixed_len, u32 size again, u16 0, u16 type.
7.  Per item: fixed part = item[0..fixed_len], heap = item[fixed_len..size];
    A_len = u32 at item+0x68; heap A = first A_len heap bytes, heap B = the
    rest (§6.3).
8.  If the type has a schema and fixed_len equals the schema's: identity is
    (type, GUID at item+12); keep one copy per identity (assumed: the copy in
    the block at the highest file offset, §6.4) and decode every property
    at its schema position (§7). A descriptor with bit 31 clear reads heap
    A, with bit 31 set heap B (§6.3).
9.  Otherwise: scan the fixed part for descriptors that point into heap B
    (§6.3) and deduplicate identical items by content (§6.5).
```

Steps 4c, 6, 8 and 9 are done under the work budgets of §6.8.

Steps 4a-4e are what make this a parser rather than a heuristic: a wrong
offset fails loudly instead of producing convincing nonsense.

---

## 9. Export database

`hxprobe export` writes a SQLite database.

**Tables**

* One **type table** per object type with a schema that occurs in the store,
  named exactly as the type (`"MessageHeader"`, `"View"`, ...; quote the name
  in SQL, `View` is a keyword). Columns: `object_id` (= `objects.id`), `guid`
  (32 lowercase hex characters, unique), `page`, then one column per property
  in `ci` order, named as in §7.2. Values: booleans 0/1; integers; `f64` as
  REAL; times as Unix seconds; strings as TEXT; bytes as TEXT when they decode
  as text (§6.7), else BLOB; GUIDs and references as hex GUID text, so a join
  is `ON a."View" = b.guid`; missing or refused values NULL. The tables
  `MessageHeader`, `MessageData`, `View`, `Recipient` and `Person` always
  exist, the others only when the store holds such an object.
* `objects`: one row per exported object: `id, type_id, type_name, guid,
  page, item_offset, fixed_len, size, content_hash, has_schema`. Schema
  objects have `guid` set and `has_schema = 1`; generic objects have `guid`
  NULL and `has_schema = 0`. Two partial unique indexes enforce one row per
  `(type_id, guid)` for schema objects and one per
  `(content_hash, size, type_id)` for generic ones.
* `fields`: one row per variable field of a generic object (§6.3):
  `object_id, at, kind, label, text, bytes, unix_time`, with `kind` one of
  `utf16`, `utf8`, `binary`. `label` and `unix_time` are always NULL in this
  version.
* `collections`: one row per node-pack collection entry (§5.2), with its
  page, type and owner.

**Views**

* `messages`: one row per `MessageHeader`, joined to its folder (`View`) and
  its `MessageData`: `guid, Subject, SenderDisplayName, SenderEmailAddress,
  SentTime, DisplayTime, IsRead, Importance, HasFileAttachment, Preview,
  InternetMessageId, folder, folder_guid, body_html, body_first,
  body_encoding`.
* `folders`: one row per `View` with its parent: `guid, Name, Type,
  TotalCount, UnreadCount, parent, parent_guid`.
* `recipients` (`object_id, page, guid, display_name, email, GivenName,
  Surname`) and `people` (`object_id, page, guid, name, email`).
* `message_headers`, `message_data`, `views`: `SELECT *` of the
  `MessageHeader`, `MessageData` and `View` tables.

`u64` page ids and hashes are stored bit-cast to signed 64-bit integers, so ids
at or above 2^63 appear negative. `objects.page` and `item_offset` are those
of the copy kept (§6.4, §6.5). The file is written under a temporary name,
put in place when complete, and has mode `0600`.

**Text with embedded NULs** (§6.7) is stored whole, but the `sqlite3` command
line tool stops at the first NUL: `length()` counts characters only up to it,
and printed values end there. To see the whole value, use `hex(col)` or
`length(CAST(col AS BLOB))` (byte length), or read the column through a client
library such as Python's `sqlite3`, which returns the full string.

---

## 10. Legacy commands

`db`, `map` and `find` read messages without the object layer: they carve
UTF-16LE text around each UTF-16LE `IPM.Note` string (the `ItemClass` of a
mail item) inside every verified, decompressed block value. The text runs they
read are the heap fields of objects such as `MessageHeader` and `MessageData`
(**Inferred**).

* Fields are **NUL-terminated UTF-16LE runs** in a stable order around the
  anchor: sender address, sender display name, `ItemClass` (the anchor),
  `InternetMessageId`, `Preview`, `NormalizedSubject`, `Topic`. Byte offsets
  shift with field lengths, so the runs are read in sequence, never at fixed
  positions. A second record layout puts the whole header *before* the
  anchor, so both sides are searched, within distance bounds (sender address
  at most 320 bytes from the anchor, display name at most 200 bytes from its
  address).
* `NormalizedSubject` and `Topic` are adjacent and near-identical (`Topic`
  keeps reply prefixes such as `Re:`); that pair identifies the subject.
  `Topic` is conversation-level, so a record without a subject can inherit one
  from a record that shares its thread identifier; such rows are flagged
  `subject_inherited`.
* Long base64 runs (`AntispamSafeLinksMsgData_Substrate`, starting `eyJ`) and
  long hex runs (`ImmId`, `ImmConversationId`, `ChangeKey`) are real fields
  but never display text.
* HTML bodies are stored as single-byte UTF-8 text after the anchor, starting
  at the first of `<html`, `<!DOCTYPE`, `<body`, `<div` or `<table`, and end at
  `</body>` or `</html>` or, for fragments, at the last `>` of an unbalanced
  `</div>` chain. Without an HTML part, the `Preview` field (about 255
  characters) is the only body text the record holds.
* The send time is the **earliest** .NET tick (§6.6) in the record span,
  between 2015-01-01 and five years after the current date.
* One message is stored many times. Identity is `sender + send time`
  (`InternetMessageId` is reused across a conversation, so it is not part of
  the key), and records with the same identity are merged field by field.
* Runs with 20 % or more characters outside Latin and punctuation are treated
  as noise, so this path is **Latin-only**; `dump` and `export` decode every
  script.

`db` writes these messages to a SQLite table `messages` with an FTS5 index
`messages_fts`; `map` prints the named field map of records (with `n = 0`,
coverage per key); `find` prints the records whose text contains a term.

---

## 11. Stability

The store format is tied to the Outlook build, and Outlook ships monthly. The
magic and version byte are the guard: `hxprobe` refuses a file without the
`Nostromo` magic, and warns on a version byte other than `i` or `h` but still
parses it, relying on the per-block checksums to fail loudly if the layout
changed (§2).

Object layouts (§7) are the part most likely to move between builds: a new
field changes `fixed_len` and can shift offsets. The schema compiled into
`hxprobe` matches 16.115.1. `hxprobe types` prints the `fixed_len` of every
type and whether the schema applies (`yes`, `size differs`, `no`), so a change
is visible before an export; items whose `fixed_len` differs from the schema
are exported generically and counted (`fixed_len mismatches`). A type whose
fields moved without a size change would be decoded wrongly.

Because the file changes while Outlook runs, any parse must be done on a copy.

---

## 12. Open questions

* **Live revision.** One key can have several verified blocks on disk. Which
  one Outlook treats as current is not known. `hxprobe` assumes the block at
  the highest file offset is live (§4.2). Decoding the store's own key index
  would settle this; that index is not decoded.
* **Which copy of an object is current.** One object is stored in several
  page values (§6.4). `hxprobe` assumes the copy in the live block with the
  highest file offset is current; this is not verified.
* **The file-header region** between `0x5000` and the first block. It is not
  a simple allocation bitmap of the block area. Its contents are not decoded.
* **Collection page body** (§5.3). The header is known; the layout of the
  index entries after `member_count` is not.
* **Bitmap B** (§6.2). Its meaning is unknown; it is all zero in every
  `MessageHeader` checked.
* **The 28 trailing bytes of a reference** (§6.4). Not decoded.
* **The rest of the 0x78-byte item header** (§6.2). Only the envelope, the
  GUID at `+12` and `A_len` at `+0x68` are known.
* **Types without a schema.** 38 of the 134 object types seen on the reference
  store have neither a name nor a schema; they hold 24 % of the items, mostly
  ids `0xd7`, `0x105` and `0xf7`. They are exported generically (§6.3).
* **Four types whose size is off by 0x10.** For `AddIn` (`0x1db`),
  `ReactionType` (`0x215`), `DoNotDisturbSettingExtension` (`0x267`) and
  `TaskAccountData` (`0x2c5`) the observed `fixed_len` is 0x10 more than the
  schema's (§6.2). Their items take the generic path. Each of the four also
  has one public property name without a known position (next item).
* **Public property names without a position.** 12 names have no position in
  the schema: `AddIn.Id`, `BestMatchSuggestion.Suggestion`,
  `CalendarSharingDetailLevel.SharingDetailLevel`,
  `CopilotPrioritizeRule.Text`, `DoNotDisturbSettingExtension.IsActive`,
  `InitializationTriggerForLocalClassification.IsInitialized`,
  `MessageRecallOption.ActorEnabled`, `PreviewICSResult.AppointmentHeaders`,
  `PreviewVCFResult.PreviewedContacts`, `ReactionType.Type`,
  `RemappedServerId.NewServerId`, `TaskAccountData.TaskFolders` (listed in
  `schema/layout.json` as `named_without_row`).
* **Hidden properties.** 1,516 of the 4,704 properties have no public name.
  Their position and type are known, their meaning is not; they are exported
  as `ci_<n>` (§7.2).
* **Body values that do not fit heap B.** 491 `MessageData` items have an
  `HTMLBodyBytes` descriptor whose range lies outside heap B (§6.3). The value
  is decoded as missing.
* **Enum values.** Properties such as `Importance`, `FlagState`,
  `View.Type` and `BodyEncoding` are exported as numbers. What each number
  means is not mapped.
* **Folder counts.** For most folders the stored `TotalCount` and
  `UnreadCount` do not match the exported messages (§7.4). The store may hold
  only part of the folder, or the counts may mean something else.
* **Other Outlook builds.** The schema matches one build (16.115.1). On
  another build, types whose size changed show `size differs` in
  `hxprobe types` and are exported generically; types whose size did not
  change but whose fields moved would be decoded wrongly. No other build has
  been tested.
* **Recipients.** `MessageData` has the references `ToRecipients`,
  `CcRecipients` and `BccRecipients` (child types `ToRecipientCollection`,
  `CcRecipientCollection`, `BccRecipientCollection`). How such a collection
  lists its `Recipient` objects, and their order, is not decoded, so
  recipients are not joined to messages.
* **Page-level structure.** The page size (4096) and region offsets are known,
  but how pages are allocated and reclaimed is not.
* **System records.** The values under the 16-byte keys (§4.1) are not
  decoded.
