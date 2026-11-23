//! Synthetic fixtures for unit tests: valid blocks and stores built in code,
//! so no real mail ever enters the repository.

use crate::block::{crc32, HEADER, MAGIC};
use crate::item::{self, TICKS_PER_SECOND, UNIX_EPOCH_TICKS};
use crate::props::PropValue;
use crate::props_gen::TYPES;
use crate::value::{ObjRef, COLLECTION_REF_TYPE, ENTRY_VERSION, MIN_ENTRY_LEN};

/// Encode `data` as a single literal-only LZ4 sequence.
///
/// Token `min(len, 15) << 4`; when `len >= 15` the 255-continuation for
/// `len - 15` always follows, ending in a byte below `0xFF` (`0x00` when
/// nothing remains, so 15 encodes as `F0 00`). Then the raw bytes.
pub fn lz4_literals(data: &[u8]) -> Vec<u8> {
    let len = data.len();
    let mut out = vec![(len.min(15) as u8) << 4];
    if len >= 15 {
        let mut rest = len - 15;
        while rest >= 255 {
            out.push(0xFF);
            rest -= 255;
        }
        out.push(rest as u8);
    }
    out.extend_from_slice(data);
    out
}

/// Assemble a block from explicit header values.
///
/// Both CRCs are computed over whatever the header says, so a test can build a
/// block that is checksum-valid but otherwise wrong (bad key length, codec or
/// inflated length). The body CRC range is clamped to the bytes written.
pub fn make_block_raw(
    key: &[u8],
    value: &[u8],
    key_len: u32,
    inflated_len: u32,
    codec: u32,
) -> Vec<u8> {
    let payload = lz4_literals(value);
    let mut b = vec![0u8; HEADER];
    b[0x08..0x10].copy_from_slice(&MAGIC.to_le_bytes());
    b[0x10..0x14].copy_from_slice(&key_len.to_le_bytes());
    b[0x14..0x18].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    b[0x18..0x1c].copy_from_slice(&inflated_len.to_le_bytes());
    b[0x1c..0x20].copy_from_slice(&codec.to_le_bytes());
    b.extend_from_slice(key);
    b.extend_from_slice(&payload);

    let end = (HEADER + key_len as usize + payload.len()).min(b.len());
    let body = crc32(&b[0x08..end]);
    b[0x04..0x08].copy_from_slice(&body.to_le_bytes());
    let head = crc32(&b[0x04..HEADER]);
    b[0x00..0x04].copy_from_slice(&head.to_le_bytes());
    b
}

/// A valid block for `key` (8 or 16 bytes) holding `value`.
///
/// `value` must be non-empty: the parser rejects `inflated_len == 0`.
pub fn make_block(key: &[u8], value: &[u8]) -> Vec<u8> {
    assert!(!value.is_empty(), "block values must be non-empty");
    make_block_raw(key, value, key.len() as u32, value.len() as u32, 4)
}

/// A store file: a `Nostromo` header (version `i`, page size 4096) followed
/// by `blocks`, each starting on a 512-byte boundary.
pub fn make_store(blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[..8].copy_from_slice(crate::header::MAGIC);
    s[8] = b'i';
    s[0x38..0x40].copy_from_slice(&4096u64.to_le_bytes());
    for b in blocks {
        s.extend_from_slice(b);
        s.resize(s.len().next_multiple_of(512), 0);
    }
    s
}

fn obj_ref_bytes(r: ObjRef) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    b.extend_from_slice(&r.prefix.to_le_bytes());
    b.extend_from_slice(&r.page.to_le_bytes());
    b.extend_from_slice(&r.type_id.to_le_bytes());
    b
}

/// A node pack value: section A with `count_a`, then section B holding
/// `entries` back to back (built with [`make_collection_entry`]).
pub fn make_node_pack(section_a: &[u8], count_a: u32, entries: &[Vec<u8>]) -> Vec<u8> {
    let section_b = entries.concat();
    let mut d = Vec::new();
    d.extend_from_slice(&count_a.to_le_bytes());
    d.extend_from_slice(&(section_a.len() as u32).to_le_bytes());
    d.extend_from_slice(section_a);
    d.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    d.extend_from_slice(&(section_b.len() as u32).to_le_bytes());
    d.extend_from_slice(&section_b);
    d
}

/// A section B entry: version 6, `0x570`, `page`, `type_id`, entry size,
/// owner ref, then `body`.
pub fn make_collection_entry(type_id: u16, page: u64, owner: ObjRef, body: &[u8]) -> Vec<u8> {
    let size = (MIN_ENTRY_LEN + body.len()) as u32;
    let mut e = Vec::new();
    e.extend_from_slice(&ENTRY_VERSION.to_le_bytes());
    e.extend_from_slice(&0x570u16.to_le_bytes());
    e.extend_from_slice(&page.to_le_bytes());
    e.extend_from_slice(&type_id.to_le_bytes());
    e.extend_from_slice(&size.to_le_bytes());
    e.extend_from_slice(&obj_ref_bytes(owner));
    e.extend_from_slice(body);
    e
}

/// A collection page value stored under `key`: `u32 1`, the self ref
/// `{prefix 0, key, 0x37}`, `member_count`, then `body`.
pub fn make_collection_page(key: u64, member_count: u32, body: &[u8]) -> Vec<u8> {
    let mut d = 1u32.to_le_bytes().to_vec();
    d.extend_from_slice(&obj_ref_bytes(ObjRef {
        prefix: 0,
        page: key,
        type_id: COLLECTION_REF_TYPE,
    }));
    d.extend_from_slice(&member_count.to_le_bytes());
    d.extend_from_slice(body);
    d
}

/// A `u32 size` prefix followed by an object item of `type_id`.
///
/// The fixed part is `fixed_len` bytes (header included, zero filled). Each
/// `(at, data)` in `vars` writes a flagged descriptor at item offset `at` and
/// appends `data` to heap B, in the order given; heap A stays empty. Each
/// `(at, ticks)` writes a `u64` at item offset `at`.
pub fn make_item(
    type_id: u16,
    fixed_len: usize,
    vars: &[(usize, &[u8])],
    ticks: &[(usize, u64)],
) -> Vec<u8> {
    make_item_heaps(type_id, fixed_len, &[], vars, ticks)
}

/// [`make_item`] with both heaps: each `(at, data)` in `vars_a` writes an
/// unflagged descriptor and appends `data` to heap A, each one in `vars_b` a
/// flagged descriptor and appends `data` to heap B. When heap A is not empty
/// its length is written to `A_len` at item offset 0x68.
pub fn make_item_heaps(
    type_id: u16,
    fixed_len: usize,
    vars_a: &[(usize, &[u8])],
    vars_b: &[(usize, &[u8])],
    ticks: &[(usize, u64)],
) -> Vec<u8> {
    assert!(fixed_len >= 16, "fixed_len must cover the item header");
    let mut item = vec![0u8; fixed_len];
    let mut heaps = [Vec::new(), Vec::new()];
    for (heap, vars, flag) in [(0, vars_a, 0), (1, vars_b, 0x8000_0000u32)] {
        let heap = &mut heaps[heap];
        for &(at, data) in vars {
            assert!(at + 8 <= fixed_len, "descriptor outside the fixed part");
            item[at..at + 4].copy_from_slice(&(heap.len() as u32).to_le_bytes());
            item[at + 4..at + 8].copy_from_slice(&(data.len() as u32 | flag).to_le_bytes());
            heap.extend_from_slice(data);
        }
    }
    for &(at, t) in ticks {
        assert!(at + 8 <= fixed_len, "ticks outside the fixed part");
        item[at..at + 8].copy_from_slice(&t.to_le_bytes());
    }
    let [heap_a, heap_b] = heaps;
    if !heap_a.is_empty() {
        assert!(fixed_len >= 0x6c, "A_len outside the fixed part");
        item[0x68..0x6c].copy_from_slice(&(heap_a.len() as u32).to_le_bytes());
    }
    item.extend_from_slice(&heap_a);
    item.extend_from_slice(&heap_b);
    let size = item.len() as u32;
    item[0..2].copy_from_slice(&5u16.to_le_bytes());
    item[2..4].copy_from_slice(&(fixed_len as u16).to_le_bytes());
    item[4..8].copy_from_slice(&size.to_le_bytes());
    item[8..10].copy_from_slice(&0u16.to_le_bytes());
    item[10..12].copy_from_slice(&type_id.to_le_bytes());
    let mut out = size.to_le_bytes().to_vec();
    out.extend_from_slice(&item);
    out
}

/// A `u32 size` prefix followed by an item of the schema type `type_name`
/// with the schema's `fixed_len` and `guid` at item offset 12.
///
/// Each `(name, value)` is written at the property's schema position: `Bool`
/// as bit `ci` of bitmap A; `Text` as a descriptor plus heap bytes (UTF-16LE
/// in heap B for dt 7, UTF-8 in heap A otherwise); `Bytes` as a descriptor
/// plus heap A bytes, or
/// raw bytes for a fixed-size property; `Int` and `Real` little-endian in the
/// property's size; `Time` (unix seconds) as .NET ticks; `Guid` as 16 bytes;
/// `Ref` as `u16 version 1, u16 type_id, guid`, zero padded to 48 bytes.
/// `Missing` and `Skipped` write nothing. Everything else stays zero.
pub fn make_typed_item(type_name: &str, guid: [u8; 16], values: &[(&str, PropValue)]) -> Vec<u8> {
    let t = TYPES
        .iter()
        .find(|t| t.name == type_name)
        .unwrap_or_else(|| panic!("no schema type {type_name}"));
    let prop = |name: &str| {
        *t.props
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("{type_name} has no property {name}"))
    };

    // Variable fields first: make_item_heaps lays out descriptors and heaps.
    // As in the store, dt 7 strings go to heap B (flagged), other variable
    // fields to heap A (unflagged).
    let mut heap_values: Vec<(bool, usize, Vec<u8>)> = Vec::new();
    for (name, v) in values {
        let p = prop(name);
        if p.size != 0 {
            continue;
        }
        let data = match v {
            PropValue::Text(s) if p.dt == 7 => utf16(s),
            PropValue::Text(s) => s.as_bytes().to_vec(),
            PropValue::Bytes(b) => b.clone(),
            PropValue::Missing | PropValue::Skipped => continue,
            other => panic!("{name}: {other:?} is not a variable field value"),
        };
        heap_values.push((p.dt == 7, p.offset as usize, data));
    }
    let vars = |in_b: bool| -> Vec<(usize, &[u8])> {
        heap_values
            .iter()
            .filter(|(b, ..)| *b == in_b)
            .map(|(_, at, d)| (*at, &d[..]))
            .collect()
    };
    let mut out = make_item_heaps(
        t.type_id,
        t.fixed_len as usize,
        &vars(false),
        &vars(true),
        &[],
    );

    // Item offset `o` is at `out[4 + o]`, after the size prefix.
    let item = &mut out[4..];
    item[12..28].copy_from_slice(&guid);
    for (name, v) in values {
        let p = prop(name);
        if p.size == 0 {
            continue;
        }
        if p.offset < 0 {
            if let PropValue::Bool(true) = v {
                let at = t.fields_end as usize + usize::from(p.ci / 8);
                item[at] |= 1 << (p.ci % 8);
            }
            continue;
        }
        let at = p.offset as usize;
        let size = usize::from(p.size);
        let bytes: Vec<u8> = match v {
            PropValue::Int(i) => i.to_le_bytes()[..size.min(8)].to_vec(),
            PropValue::Real(f) => f.to_le_bytes().to_vec(),
            PropValue::Time(s) => (UNIX_EPOCH_TICKS as i64 + s * TICKS_PER_SECOND as i64)
                .to_le_bytes()
                .to_vec(),
            PropValue::Guid(g) => g.to_vec(),
            PropValue::Ref { type_id, guid } => {
                let mut r = vec![0u8; 48];
                r[0..2].copy_from_slice(&1u16.to_le_bytes());
                r[2..4].copy_from_slice(&type_id.to_le_bytes());
                r[4..20].copy_from_slice(guid);
                r
            }
            PropValue::Bytes(b) => b.clone(),
            PropValue::Missing | PropValue::Skipped | PropValue::Bool(false) => continue,
            other => panic!("{name}: {other:?} does not fit size {size}"),
        };
        assert!(bytes.len() <= size, "{name}: value wider than the property");
        item[at..at + bytes.len()].copy_from_slice(&bytes);
    }
    out
}

/// Overwrite the envelope `fixed_len` of a prefixed item (as built by
/// [`make_item`] or [`make_typed_item`]); the bytes stay where they are.
pub fn set_fixed_len(v: &mut [u8], fixed_len: u16) {
    v[6..8].copy_from_slice(&fixed_len.to_le_bytes());
}

/// Copy the 8-byte descriptor at item offset `from` of a prefixed item to
/// item offset `to`, so both name the same heap bytes.
pub fn copy_descriptor(v: &mut [u8], from: usize, to: usize) {
    let d: [u8; 8] = v[4 + from..4 + from + 8].try_into().unwrap();
    v[4 + to..4 + to + 8].copy_from_slice(&d);
}

/// 2024-03-01 00:00:00 UTC in .NET ticks.
pub const T2024: u64 = UNIX_EPOCH_TICKS + 1_709_251_200 * TICKS_PER_SECOND;

/// The fixed "current time" of the tests, 2026-01-01 00:00:00 UTC, so no
/// test depends on the wall clock.
pub const NOW: u64 = 1_767_225_600;

/// The tick window at [`NOW`].
pub fn window() -> (u64, u64) {
    item::tick_window_at(NOW)
}

/// `s` as UTF-16LE bytes.
pub fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// The 8-byte block key of page `k`.
pub fn page(k: u64) -> [u8; 8] {
    k.to_le_bytes()
}

/// A Recipient item with the schema's fixed_len (0x15a): name, email and a 2024 tick.
pub fn recipient(name: &str, email: &str) -> Vec<u8> {
    make_item(
        0xd2,
        0x15a,
        &[(0x100, &utf16(name)), (0x108, &utf16(email))],
        &[(0xf8, T2024)],
    )
}

/// A value of `len` bytes (a multiple of 16) with an item envelope of type
/// 0x56 every 16 bytes, each claiming everything up to the end of the value:
/// `len / 16 - 1` valid, overlapping items of about `len^2 / 32` bytes in all.
pub fn overlapping_envelopes(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for p in (0..len - 16).step_by(16) {
        let size = (len - p - 4) as u32;
        v[p..p + 4].copy_from_slice(&size.to_le_bytes());
        v[p + 4..p + 6].copy_from_slice(&[5, 0]);
        v[p + 6..p + 8].copy_from_slice(&16u16.to_le_bytes());
        v[p + 8..p + 12].copy_from_slice(&size.to_le_bytes());
        v[p + 14..p + 16].copy_from_slice(&0x56u16.to_le_bytes());
    }
    v
}

/// A store of `n` 512-byte slots, each opening with a block header whose
/// header checksum is valid and whose value is claimed to run to the end of
/// the file. No body checksum agrees, so no block verifies, but checking them
/// all would checksum about `n / 2` times the file length.
pub fn overlapping_blocks(n: usize) -> Vec<u8> {
    let mut s = make_store(&[]);
    s.resize(512 + n * 512, 0);
    let len = s.len();
    for off in (512..len).step_by(512) {
        let val_len = (len - off - HEADER - 8) as u32;
        s[off + 0x08..off + 0x10].copy_from_slice(&MAGIC.to_le_bytes());
        s[off + 0x10..off + 0x14].copy_from_slice(&8u32.to_le_bytes());
        s[off + 0x14..off + 0x18].copy_from_slice(&val_len.to_le_bytes());
        s[off + 0x18..off + 0x1c].copy_from_slice(&1u32.to_le_bytes());
        s[off + 0x1c..off + 0x20].copy_from_slice(&4u32.to_le_bytes());
        let head = crc32(&s[off + 0x04..off + HEADER]);
        s[off..off + 4].copy_from_slice(&head.to_le_bytes());
    }
    s
}

/// GUIDs of the [`mail_store`] fixture.
pub const MAIL_VIEW_TOP: [u8; 16] = [0x10; 16];
pub const MAIL_VIEW_INBOX: [u8; 16] = [0x11; 16];
/// Messages: report (HTML body, two attachments), Hebrew (HTML body),
/// binary (body bytes that are not UTF-8), no body (no MessageData).
pub const MAIL_MSGS: [[u8; 16]; 4] = [[0x21; 16], [0x22; 16], [0x23; 16], [0x24; 16]];
pub const MAIL_DATA: [[u8; 16]; 3] = [[0x31; 16], [0x32; 16], [0x33; 16]];
pub const MAIL_ATTACHMENTS: [[u8; 16]; 2] = [[0x41; 16], [0x42; 16]];
pub const MAIL_PEOPLE: [[u8; 16]; 3] = [[0x51; 16], [0x52; 16], [0x53; 16]];
/// Appointments: planning (2026-01-05 10:00..11:00 UTC) and a cancelled
/// review (2026-01-07 14:00..15:30 UTC).
pub const MAIL_EVENTS: [[u8; 16]; 2] = [[0x61; 16], [0x62; 16]];
/// `(start, end)` unix seconds of [`MAIL_EVENTS`].
pub const MAIL_EVENT_TIMES: [(i64, i64); 2] = [
    (1_767_607_200, 1_767_610_800),
    (1_767_794_400, 1_767_799_800),
];
/// Sent times of [`MAIL_MSGS`]: 2025-12-01 09:00, 2025-12-10 12:00,
/// 2025-12-15 08:30 and 2025-11-20 17:45 UTC.
pub const MAIL_SENT: [i64; 4] = [1_764_579_600, 1_765_368_000, 1_765_787_400, 1_763_660_700];

/// The body of the binary message: odd length and not UTF-8, so the export
/// writes it as a BLOB.
pub const MAIL_BINARY_BODY: &[u8] = b"<p>caf\xe9 zebra</p>";

const MESSAGE_HEADER: u16 = 0xc9;
const MESSAGE_DATA: u16 = 0xca;
const VIEW: u16 = 0x4d;

fn text(s: &str) -> PropValue {
    PropValue::Text(s.into())
}

fn reference(type_id: u16, guid: [u8; 16]) -> PropValue {
    PropValue::Ref { type_id, guid }
}

/// A 16-byte `TimeRangeUtc` value: start and end as little-endian ticks.
fn time_range(start: i64, end: i64) -> PropValue {
    let ticks = |s: i64| UNIX_EPOCH_TICKS + s as u64 * TICKS_PER_SECOND;
    let mut b = ticks(start).to_le_bytes().to_vec();
    b.extend_from_slice(&ticks(end).to_le_bytes());
    PropValue::Bytes(b)
}

/// Items of the [`mail_store`] fixture without attachments and appointments.
fn mail_items() -> Vec<Vec<u8>> {
    let views = [
        make_typed_item(
            "View",
            MAIL_VIEW_TOP,
            &[
                ("Name", text("Top")),
                ("TotalCount", PropValue::Int(2)),
                ("UnreadCount", PropValue::Int(1)),
            ],
        ),
        make_typed_item(
            "View",
            MAIL_VIEW_INBOX,
            &[
                ("Name", text("Inbox")),
                ("TotalCount", PropValue::Int(2)),
                ("UnreadCount", PropValue::Int(1)),
                ("ParentView", reference(VIEW, MAIL_VIEW_TOP)),
            ],
        ),
    ];
    // (subject, sender name, address, read, attachment, folder, preview, data)
    type Msg = (
        &'static str,
        &'static str,
        &'static str,
        bool,
        bool,
        [u8; 16],
        &'static str,
        Option<[u8; 16]>,
    );
    let msgs: [Msg; 4] = [
        (
            "Quarterly report",
            "Ann Lee",
            "ann@example.org",
            true,
            true,
            MAIL_VIEW_INBOX,
            "The budget numbers",
            Some(MAIL_DATA[0]),
        ),
        (
            "שלום עולם",
            "Dana Cohen",
            "dana@example.com",
            false,
            false,
            MAIL_VIEW_INBOX,
            "פגישה",
            Some(MAIL_DATA[1]),
        ),
        (
            "Binary body",
            "Bob Roe",
            "bob@example.org",
            false,
            false,
            MAIL_VIEW_TOP,
            "Some bytes",
            Some(MAIL_DATA[2]),
        ),
        (
            "No body here",
            "Carl Poe",
            "carl@example.com",
            true,
            false,
            MAIL_VIEW_TOP,
            "Nothing",
            None,
        ),
    ];
    let headers = msgs
        .iter()
        .zip(MAIL_MSGS)
        .zip(MAIL_SENT)
        .map(|((m, guid), sent)| {
            let mut v = vec![
                ("Subject", text(m.0)),
                ("SenderDisplayName", text(m.1)),
                ("SenderEmailAddress", text(m.2)),
                ("IsRead", PropValue::Bool(m.3)),
                ("HasFileAttachment", PropValue::Bool(m.4)),
                ("View", reference(VIEW, m.5)),
                ("Preview", text(m.6)),
                ("SentTime", PropValue::Time(sent)),
                ("InternetMessageId", text("<id@example.com>")),
            ];
            if let Some(d) = m.7 {
                v.push(("MessageData", reference(MESSAGE_DATA, d)));
            }
            make_typed_item("MessageHeader", guid, &v)
        });
    let bodies: [&[u8]; 3] = [
        b"<html><body><p>The <b>budget</b> numbers are attached.</p></body></html>",
        "<div dir=\"rtl\">פגישה מחר בבוקר</div>".as_bytes(),
        MAIL_BINARY_BODY,
    ];
    let data = bodies.iter().zip(MAIL_DATA).map(|(b, guid)| {
        make_typed_item(
            "MessageData",
            guid,
            &[("HTMLBodyBytes", PropValue::Bytes(b.to_vec()))],
        )
    });
    let people = [
        ("Ann Lee", "ann@example.org"),
        ("Dana Cohen", "dana@example.com"),
        ("Eve 100% Diaz", "eve_d@example.org"),
    ]
    .iter()
    .zip(MAIL_PEOPLE)
    .map(|((name, addr), guid)| {
        make_typed_item(
            "Person",
            guid,
            &[("DisplayName", text(name)), ("EmailAddress", text(addr))],
        )
    });
    views
        .into_iter()
        .chain(headers)
        .chain(data)
        .chain(people)
        .collect()
}

/// Attachment and appointment items of the [`mail_store`] fixture.
fn calendar_items() -> Vec<Vec<u8>> {
    let attachments = [
        ("report.pdf", "Report", "application/pdf", 12_345),
        ("chart.png", "Chart", "image/png", 2_048),
    ]
    .iter()
    .zip(MAIL_ATTACHMENTS)
    .map(|((file, name, ct, size), guid)| {
        make_typed_item(
            "AttachmentHeader",
            guid,
            &[
                ("Filename", text(file)),
                ("DisplayName", text(name)),
                ("ContentType", text(ct)),
                ("Size", PropValue::Int(*size)),
                ("MessageHeader", reference(MESSAGE_HEADER, MAIL_MSGS[0])),
            ],
        )
    });
    let events = [("Planning", "Room 1", false), ("Review", "Room 2", true)]
        .iter()
        .zip(MAIL_EVENTS)
        .zip(MAIL_EVENT_TIMES)
        .map(|(((subject, location, cancelled), guid), (start, end))| {
            make_typed_item(
                "AppointmentHeader",
                guid,
                &[
                    ("NormalizedSubject", text(subject)),
                    ("Location", text(location)),
                    ("IsCancelled", PropValue::Bool(*cancelled)),
                    ("Organizer_DisplayName", text("Ann Lee")),
                    ("Organizer_EmailAddress", text("ann@example.org")),
                    ("BodyPreview", text("Agenda")),
                    ("TimeRangeUtc", time_range(start, end)),
                ],
            )
        });
    attachments.chain(events).collect()
}

/// A store of `items`, one block per item on pages 1, 2, ...
fn store_of(items: Vec<Vec<u8>>) -> Vec<u8> {
    let blocks: Vec<Vec<u8>> = items
        .iter()
        .enumerate()
        .map(|(i, it)| make_block(&page(i as u64 + 1), it))
        .collect();
    make_store(&blocks)
}

/// A synthetic mailbox: 2 folders (`Top` holds `Inbox`), 4 messages, 3
/// bodies, 2 attachments of the first message, 3 people and 2 appointments
/// (the second cancelled). GUIDs are the `MAIL_*` constants.
pub fn mail_store() -> Vec<u8> {
    let mut items = mail_items();
    items.extend(calendar_items());
    store_of(items)
}

/// [`mail_store`] without attachments and appointments.
pub fn mail_store_without_calendar() -> Vec<u8> {
    store_of(mail_items())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_encoder_length_continuations() {
        assert_eq!(lz4_literals(&[1; 14])[0], 0xe0);
        assert_eq!(&lz4_literals(&[1; 15])[..2], &[0xf0, 0x00]);
        assert_eq!(&lz4_literals(&[1; 270])[..3], &[0xf0, 0xff, 0x00]);
        for len in [1, 14, 15, 16, 269, 270, 271, 600] {
            let v: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(crate::lz::decode_exact(&lz4_literals(&v), len).unwrap(), v);
        }
    }

    #[test]
    fn mail_store_exports_every_object() {
        for (mm, objects) in [(mail_store(), 16), (mail_store_without_calendar(), 12)] {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            let st =
                crate::export::export(&crate::kv::Store::load(&mm), &mm, &conn, window()).unwrap();
            assert_eq!(st.schema_objects, objects);
            assert_eq!((st.fixed_len_mismatch, st.no_guid), (0, 0));
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE folder IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 4);
            let blob: String = conn
                .query_row(
                    "SELECT typeof(body_html) FROM messages WHERE Subject = 'Binary body'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(blob, "blob");
        }
    }

    #[test]
    fn store_has_a_valid_header() {
        let s = make_store(&[]);
        let h = crate::header::check(&s).unwrap();
        assert!(h.known);
        assert_eq!(h.page_size, 4096);
    }
}
