//! Outlook's own property schema: named, typed properties per object type.
//!
//! The tables live in the generated [`crate::props_gen`] (from
//! `schema/layout.json`). Offsets are relative to the item start (its
//! `05 00`); booleans have no offset and are bit `ci` of bitmap A, which
//! starts at `fields_end`.

use crate::item::{self, FieldBudget, Item, Text};
use crate::le::{u16_at, u32_at};
use crate::props_gen::TYPES;

/// Item offset of an item's own GUID.
const GUID_AT: usize = 12;
/// Data type of a UTF-16LE string (variable field).
const DT_UTF16: u8 = 7;
/// Data type of a signed 32-bit integer.
const DT_I32: u8 = 2;
/// Data type of a .NET ticks date-time.
const DT_TICKS: u8 = 6;
/// Data type of an `f64`.
const DT_F64: u8 = 0x15;
/// Size of a variable field descriptor in the fixed part.
const DESC_LEN: usize = 8;
/// Size of an object reference.
const REF_LEN: usize = 48;

/// One property of an object type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prop {
    /// Change index: position in the type's alphabetical property list,
    /// hidden properties included.
    pub ci: u16,
    /// Property id; 0 = unknown.
    pub pid: u16,
    /// Public name; "" = hidden.
    pub name: &'static str,
    /// Item offset; -1 = boolean in bitmap A.
    pub offset: i32,
    /// Size in the fixed part; 0 = variable field descriptor (8 bytes).
    pub size: u8,
    /// Data type (low byte of the schema flags).
    pub dt: u8,
    /// Member type of a collection reference; 0 when none.
    pub child_type: u16,
}

/// Builds a [`Prop`]; keeps `props_gen.rs` at one line per property.
pub const fn p(
    ci: u16,
    pid: u16,
    name: &'static str,
    offset: i32,
    size: u8,
    dt: u8,
    child_type: u16,
) -> Prop {
    Prop {
        ci,
        pid,
        name,
        offset,
        size,
        dt,
        child_type,
    }
}

/// The schema of one object type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeSchema {
    pub type_id: u16,
    pub name: &'static str,
    /// Item offset where bitmap A starts.
    pub fields_end: u32,
    /// Number of change indexes; each bitmap is `ceil(ci_count / 8)` bytes.
    pub ci_count: u16,
    /// Expected envelope `fixed_len`: `fields_end + 2 * ceil(ci_count / 8)`.
    pub fixed_len: u32,
    /// Strictly sorted by `ci`.
    pub props: &'static [Prop],
}

/// The decoded value of one property.
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    /// Not set, zero, or out of bounds.
    Missing,
    /// Variable field refused by the item's [`FieldBudget`].
    Skipped,
    Bool(bool),
    Int(i64),
    Real(f64),
    /// Unix seconds.
    Time(i64),
    Text(String),
    Bytes(Vec<u8>),
    Guid([u8; 16]),
    Ref {
        type_id: u16,
        guid: [u8; 16],
    },
}

/// The schema of `type_id`, if Outlook's schema has one.
pub fn schema_for(type_id: u16) -> Option<&'static TypeSchema> {
    TYPES
        .binary_search_by_key(&type_id, |t| t.type_id)
        .ok()
        .map(|i| &TYPES[i])
}

/// True when `item` has the struct size of `schema`; only then are the
/// schema's offsets valid for it (another Outlook build may differ).
pub fn applies(schema: &TypeSchema, item: &Item) -> bool {
    item.fixed_len == schema.fixed_len as usize
}

/// The item's own GUID (bytes 12..28); `None` when all zero or the item is
/// too short.
pub fn guid_of(item: &Item) -> Option<[u8; 16]> {
    nonzero_guid(item.bytes, GUID_AT)
}

/// 32 lowercase hex chars, bytes in stored order.
pub fn guid_hex(g: &[u8; 16]) -> String {
    g.iter().map(|b| format!("{b:02x}")).collect()
}

fn nonzero_guid(b: &[u8], at: usize) -> Option<[u8; 16]> {
    let g: [u8; 16] = b.get(at..at.checked_add(16)?)?.try_into().ok()?;
    (g != [0; 16]).then_some(g)
}

/// UTF-16LE, lossy, trailing NULs stripped (an odd last byte is ignored).
fn utf16_lossy(b: &[u8]) -> String {
    let mut units: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    String::from_utf16_lossy(&units)
}

/// Decode property `p` of `item` (see SPEC.md, "Property schema").
///
/// Every read is bounds-checked against the fixed part; anything out of
/// range is [`PropValue::Missing`]. A variable field the item's `budget`
/// refuses is [`PropValue::Skipped`] (and counted in the budget). `window`
/// is the run's tick window ([`item::tick_window`]). Variable fields are read
/// with [`Item::var_at`]: heap A when the descriptor is unflagged, heap B
/// when it is flagged.
pub fn decode(
    schema: &TypeSchema,
    item: &Item,
    p: &Prop,
    window: (u64, u64),
    budget: &mut FieldBudget,
) -> PropValue {
    // The fixed part as far as the item bytes really hold it.
    if item.fixed_len > item.bytes.len() {
        return PropValue::Missing;
    }
    let fixed = &item.bytes[..item.fixed_len];

    if p.offset < 0 {
        let at = schema.fields_end as usize + usize::from(p.ci / 8);
        return match fixed.get(at) {
            Some(b) => PropValue::Bool((b >> (p.ci % 8)) & 1 == 1),
            None => PropValue::Missing,
        };
    }
    let at = p.offset as usize;
    let width = if p.size == 0 {
        DESC_LEN
    } else {
        usize::from(p.size)
    };
    let Some(raw) = at.checked_add(width).and_then(|end| fixed.get(at..end)) else {
        return PropValue::Missing;
    };

    match p.size {
        0 => {
            let Some(data) = item.var_at(at) else {
                return PropValue::Missing;
            };
            if !budget.take(data.len()) {
                return PropValue::Skipped;
            }
            if p.dt == DT_UTF16 {
                return PropValue::Text(utf16_lossy(data));
            }
            match item::decode_text(data) {
                Text::Utf16(s) | Text::Utf8(s) => PropValue::Text(s),
                Text::Binary => PropValue::Bytes(data.to_vec()),
            }
        }
        1 => PropValue::Int(i64::from(raw[0])),
        2 => u16_at(raw, 0).map_or(PropValue::Missing, |v| PropValue::Int(i64::from(v))),
        4 => match u32_at(raw, 0) {
            Some(v) if p.dt == DT_I32 => PropValue::Int(i64::from(v as i32)),
            Some(v) => PropValue::Int(i64::from(v)),
            None => PropValue::Missing,
        },
        8 => {
            let b: [u8; 8] = raw.try_into().expect("8-byte slice");
            match p.dt {
                DT_TICKS => item
                    .ticks_at(at, window)
                    .map_or(PropValue::Missing, PropValue::Time),
                DT_F64 => PropValue::Real(f64::from_le_bytes(b)),
                _ => PropValue::Int(i64::from_le_bytes(b)),
            }
        }
        16 => nonzero_guid(raw, 0).map_or(PropValue::Missing, PropValue::Guid),
        s if usize::from(s) == REF_LEN => match (u16_at(raw, 2), nonzero_guid(raw, 4)) {
            (Some(type_id), Some(guid)) => PropValue::Ref { type_id, guid },
            _ => PropValue::Missing,
        },
        _ => PropValue::Bytes(raw.to_vec()),
    }
}

/// SQL column name of a property: its name, else `prop_<pid>`, else
/// `ci_<ci>`.
pub fn column_name(p: &Prop) -> String {
    if !p.name.is_empty() {
        p.name.to_string()
    } else if p.pid != 0 {
        format!("prop_{}", p.pid)
    } else {
        format!("ci_{}", p.ci)
    }
}

/// Quote an SQL identifier: `"name"` with every embedded `"` doubled. The
/// only way identifiers reach SQL.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names;
    use crate::testutil::{make_typed_item, window};
    use std::collections::HashSet;

    fn ty(name: &str) -> &'static TypeSchema {
        TYPES.iter().find(|t| t.name == name).unwrap()
    }

    fn prop(t: &TypeSchema, name: &str) -> Prop {
        *t.props.iter().find(|p| p.name == name).unwrap()
    }

    /// `(name, item offset, size, dt)` must match.
    fn check(t: &TypeSchema, rows: &[(&str, i32, u8, u8)]) {
        for &(name, offset, size, dt) in rows {
            let p = prop(t, name);
            assert_eq!(
                (p.offset, p.size, p.dt),
                (offset, size, dt),
                "{}.{name}",
                t.name
            );
        }
    }

    fn ci_of(t: &TypeSchema, name: &str) -> u16 {
        let p = prop(t, name);
        assert_eq!(p.offset, -1, "{} is a boolean", name);
        p.ci
    }

    #[test]
    fn table_counts_and_order() {
        assert_eq!(TYPES.len(), 219);
        assert!(TYPES.windows(2).all(|w| w[0].type_id < w[1].type_id));
        let props: usize = TYPES.iter().map(|t| t.props.len()).sum();
        assert_eq!(props, 4704);
        let named = TYPES
            .iter()
            .flat_map(|t| t.props)
            .filter(|p| !p.name.is_empty())
            .count();
        assert_eq!(named, 3188);
        for t in &TYPES {
            assert!(t.props.windows(2).all(|w| w[0].ci < w[1].ci), "{}", t.name);
            assert!(t.props.iter().all(|p| p.ci < t.ci_count), "{}", t.name);
        }
    }

    #[test]
    fn offsets_inside_the_fields() {
        for t in &TYPES {
            for p in t.props.iter().filter(|p| p.offset != -1) {
                let width = if p.size == 0 { 8 } else { u32::from(p.size) };
                assert!(p.offset >= 0x78, "{}.ci{}", t.name, p.ci);
                assert!(
                    p.offset as u32 + width <= t.fields_end,
                    "{}.ci{}",
                    t.name,
                    p.ci
                );
            }
        }
    }

    #[test]
    fn fixed_len_formula() {
        for t in &TYPES {
            let bitmap = u32::from(t.ci_count).div_ceil(8);
            assert_eq!(t.fixed_len, t.fields_end + 2 * bitmap, "{}", t.name);
        }
    }

    #[test]
    fn names_match_type_table() {
        for t in &TYPES {
            assert_eq!(
                names::type_name(t.type_id),
                Some(t.name),
                "{:#x}",
                t.type_id
            );
        }
    }

    #[test]
    fn message_header_checked_values() {
        let t = ty("MessageHeader");
        assert_eq!(
            (t.type_id, t.fixed_len, t.fields_end, t.ci_count),
            (0xc9, 0x60f, 0x5e1, 178)
        );
        check(
            t,
            &[
                ("Subject", 0x598, 0, 7),
                ("Preview", 0x518, 0, 7),
                ("NormalizedSubject", 0x4ec, 0, 7),
                ("SenderDisplayName", 0x56c, 0, 7),
                ("SenderEmailAddress", 0x574, 0, 7),
                ("InternetMessageId", 0x4cc, 0, 7),
                ("ItemClass", 0x4d4, 0, 7),
                // Variable byte field (dt 9), not text.
                ("ConversationIndex", 0x498, 0, 9),
                ("SentTime", 0x2d8, 8, 6),
                ("DisplayTime", 0x120, 8, 6),
                ("LastModifiedTime", 0x178, 8, 6),
                ("SortTime", 0x340, 8, 6),
                ("Importance", 0x4b8, 4, 1),
                ("Size", 0x594, 4, 2),
                ("FlagState", 0x5cc, 1, 0x10),
                ("View", 0x380, 48, 0x0a),
                ("MessageData", 0x1e8, 48, 0x0a),
            ],
        );
        assert_eq!(ci_of(t, "IsRead"), 73);
        assert_eq!(ci_of(t, "HasFileAttachment"), 46);
        assert_eq!(ci_of(t, "IsDraft"), 64);
    }

    #[test]
    fn view_person_message_data_recipient_checked_values() {
        let v = ty("View");
        assert_eq!(
            (v.type_id, v.fixed_len, v.fields_end, v.ci_count),
            (0x4d, 0x4d8, 0x4b4, 139)
        );
        check(
            v,
            &[
                ("Name", 0x448, 0, 7),
                ("Type", 0x4a8, 4, 2),
                ("TotalCount", 0x4a0, 4, 1),
                ("UnreadCount", 0x4ac, 4, 1),
                ("ParentView", 0x320, 48, 0x0a),
                ("Account", 0x78, 48, 0x0a),
            ],
        );

        let pe = ty("Person");
        assert_eq!((pe.type_id, pe.fixed_len), (0x55, 0x15e));
        check(
            pe,
            &[("DisplayName", 0x124, 0, 7), ("EmailAddress", 0x12c, 0, 7)],
        );

        let d = ty("MessageData");
        assert_eq!(
            (d.type_id, d.fixed_len, d.fields_end, d.ci_count),
            (0xca, 0x752, 0x730, 132)
        );
        check(
            d,
            &[
                ("HTMLBodyBytes", 0x67c, 0, 0x0f),
                ("HTMLFirstBodyBytes", 0x684, 0, 0x0f),
                ("IpmClassName", 0x68c, 0, 7),
                ("BodyEncoding", 0x610, 4, 2),
                ("ToRecipients", 0x570, 48, 0x0a),
            ],
        );

        let r = ty("Recipient");
        assert_eq!(
            (r.type_id, r.fixed_len, r.fields_end, r.ci_count),
            (0xd2, 0x15a, 0x154, 24)
        );
        check(
            r,
            &[
                ("DisplayName", 0x100, 0, 7),
                ("EmailAddress", 0x108, 0, 7),
                ("EmailType", 0x110, 4, 1),
                ("GivenName", 0x114, 0, 7),
                ("ReferenceId", 0x134, 0, 7),
                // Variable byte field (dt 9), not text.
                ("ServerId", 0x140, 0, 9),
                ("SubType", 0x148, 4, 1),
                ("Surname", 0x14c, 0, 7),
            ],
        );
    }

    #[test]
    fn column_names_are_sql_safe() {
        for t in &TYPES {
            let mut seen = HashSet::new();
            for p in t.props {
                let c = column_name(p).to_ascii_lowercase();
                assert!(
                    !["object_id", "guid", "page"].contains(&c.as_str()),
                    "{}.{c}",
                    t.name
                );
                assert!(seen.insert(c.clone()), "{}: duplicate column {c}", t.name);
            }
            assert!(t.props.len() + 3 <= 2000, "{}: too many columns", t.name);
        }
    }

    #[test]
    fn type_names_are_sql_safe() {
        // Tables, views and indexes share one namespace.
        const RESERVED: [&str; 14] = [
            "objects",
            "fields",
            "collections",
            "objects_content",
            "objects_guid",
            "fields_object",
            "objects_type",
            "messages",
            "folders",
            "recipients",
            "people",
            "message_headers",
            "message_data",
            "views",
        ];
        // SQLite table names are case-insensitive.
        let mut seen = HashSet::new();
        for t in &TYPES {
            assert!(
                seen.insert(t.name.to_ascii_lowercase()),
                "type names differ only by case: {}",
                t.name
            );
            let b = t.name.as_bytes();
            assert!(!b.is_empty(), "empty type name");
            assert!(b[0].is_ascii_alphabetic() || b[0] == b'_', "{}", t.name);
            assert!(
                b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_'),
                "{}",
                t.name
            );
            assert!(
                !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(t.name)),
                "{}",
                t.name
            );
        }
    }

    #[test]
    fn column_name_fallbacks() {
        assert_eq!(column_name(&p(1, 5, "Subject", 0x78, 0, 7, 0)), "Subject");
        assert_eq!(column_name(&p(2, 77, "", 0x78, 4, 1, 0)), "prop_77");
        assert_eq!(column_name(&p(3, 0, "", -1, 1, 3, 0)), "ci_3");
    }

    #[test]
    fn quote_ident_doubles_quotes() {
        assert_eq!(quote_ident("View"), "\"View\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident(""), "\"\"");
    }

    const GUID: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32,
        0x10,
    ];
    const VIEW_GUID: [u8; 16] = [0xaa; 16];

    /// Decode property `name` of `it` with a fresh budget.
    fn get(t: &TypeSchema, it: &Item, name: &str) -> PropValue {
        let mut b = FieldBudget::new(it);
        decode(t, it, &prop(t, name), window(), &mut b)
    }

    fn header() -> Vec<u8> {
        make_typed_item(
            "MessageHeader",
            GUID,
            &[
                ("Subject", PropValue::Text("שלום עולם".into())),
                ("SentTime", PropValue::Time(1_709_251_200)),
                ("IsRead", PropValue::Bool(true)),
                ("Importance", PropValue::Int(2)),
                (
                    "View",
                    PropValue::Ref {
                        type_id: 0x4d,
                        guid: VIEW_GUID,
                    },
                ),
            ],
        )
    }

    #[test]
    fn message_header_round_trip() {
        let v = header();
        let it = &item::scan(&v)[0];
        let t = schema_for(0xc9).unwrap();
        assert!(applies(t, it));
        assert_eq!(guid_of(it), Some(GUID));
        assert_eq!(get(t, it, "Subject"), PropValue::Text("שלום עולם".into()));
        assert_eq!(get(t, it, "SentTime"), PropValue::Time(1_709_251_200));
        assert_eq!(get(t, it, "IsRead"), PropValue::Bool(true));
        assert_eq!(get(t, it, "Importance"), PropValue::Int(2));
        assert_eq!(
            get(t, it, "View"),
            PropValue::Ref {
                type_id: 0x4d,
                guid: VIEW_GUID
            }
        );
        // Unset string, zero ticks, zero ref, clear bit.
        assert_eq!(get(t, it, "Preview"), PropValue::Missing);
        assert_eq!(get(t, it, "DisplayTime"), PropValue::Missing);
        assert_eq!(get(t, it, "MessageData"), PropValue::Missing);
        assert_eq!(get(t, it, "IsDraft"), PropValue::Bool(false));
        assert_eq!(get(t, it, "HasFileAttachment"), PropValue::Bool(false));
    }

    #[test]
    fn signed_real_guid_and_byte_fields() {
        let v = make_typed_item(
            "MessageHeader",
            GUID,
            &[
                ("Size", PropValue::Int(-5)),
                ("FlagState", PropValue::Int(0xfe)),
                (
                    "ConversationIndex",
                    PropValue::Bytes(vec![1, 0xe0, 2, 0xe0]),
                ),
                ("ItemClass", PropValue::Text("IPM.Note".into())),
            ],
        );
        let it = &item::scan(&v)[0];
        let t = schema_for(0xc9).unwrap();
        assert_eq!(get(t, it, "Size"), PropValue::Int(-5));
        assert_eq!(get(t, it, "FlagState"), PropValue::Int(0xfe));
        assert_eq!(
            get(t, it, "ConversationIndex"),
            PropValue::Bytes(vec![1, 0xe0, 2, 0xe0])
        );
        assert_eq!(get(t, it, "ItemClass"), PropValue::Text("IPM.Note".into()));

        // A text-like value in a bytes-typed field (dt 9) decodes as Text.
        let v = make_typed_item(
            "Recipient",
            GUID,
            &[("ServerId", PropValue::Text("server-1".into()))],
        );
        let it = &item::scan(&v)[0];
        let r = schema_for(0xd2).unwrap();
        assert_eq!(get(r, it, "ServerId"), PropValue::Text("server-1".into()));

        // Synthetic 8-byte f64, 16-byte GUID and odd-size props.
        let it = &item::scan(&v)[0];
        let mut b = FieldBudget::new(it);
        let mut fixed =
            |offset, size, dt| decode(r, it, &p(0, 0, "", offset, size, dt, 0), window(), &mut b);
        assert_eq!(fixed(12, 16, 0x0b), PropValue::Guid(GUID));
        assert_eq!(fixed(0x20, 16, 0x0b), PropValue::Missing);
        assert_eq!(fixed(0x20, 8, 0x15), PropValue::Real(0.0));
        assert_eq!(fixed(0x20, 8, 0x05), PropValue::Int(0));
        assert_eq!(fixed(12, 3, 0), PropValue::Bytes(GUID[..3].to_vec()));
    }

    #[test]
    fn applies_needs_the_schema_fixed_len() {
        let t = schema_for(0xc9).unwrap();
        let mut v = header();
        let short = (t.fixed_len - 2) as u16;
        v[6..8].copy_from_slice(&short.to_le_bytes());
        let it = &item::scan(&v)[0];
        assert_eq!(it.fixed_len, t.fixed_len as usize - 2);
        assert!(!applies(t, it));
    }

    #[test]
    fn truncated_item_and_bad_descriptor_are_missing() {
        let t = schema_for(0xc9).unwrap();
        let v = header();
        let full = &item::scan(&v)[0];
        // An item whose fixed part ends before the property offsets.
        let it = Item {
            fixed_len: 0x100,
            bytes: &full.bytes[..0x100],
            ..full.clone()
        };
        for name in ["Subject", "SentTime", "IsRead", "Importance", "View"] {
            assert_eq!(get(t, &it, name), PropValue::Missing, "{name}");
        }
        // fixed_len beyond the item bytes: no panic.
        let it = Item {
            bytes: &full.bytes[..0x100],
            ..full.clone()
        };
        let mut b = FieldBudget::new(full);
        for name in ["Subject", "IsRead"] {
            let got = decode(t, &it, &prop(t, name), window(), &mut b);
            assert_eq!(got, PropValue::Missing, "{name}");
        }

        // Subject's descriptor names one byte past the heap.
        let mut v = header();
        let at = 4 + prop(t, "Subject").offset as usize + 4;
        let heap_len = full.heap().len() as u32;
        v[at..at + 4].copy_from_slice(&((heap_len + 1) | 0x8000_0000).to_le_bytes());
        let it = &item::scan(&v)[0];
        assert_eq!(get(t, it, "Subject"), PropValue::Missing);
    }

    #[test]
    fn strings_sharing_one_heap_stop_at_the_budget() {
        let t = schema_for(0xc9).unwrap();
        let mut v = header();
        let names = [
            "Subject",
            "Preview",
            "NormalizedSubject",
            "SenderDisplayName",
            "SenderEmailAddress",
            "InternetMessageId",
        ];
        // Point every descriptor at Subject's heap bytes (the whole heap).
        let src = 4 + prop(t, "Subject").offset as usize;
        let desc: [u8; 8] = v[src..src + 8].try_into().unwrap();
        for name in &names[1..] {
            let at = 4 + prop(t, name).offset as usize;
            v[at..at + 8].copy_from_slice(&desc);
        }
        let it = &item::scan(&v)[0];
        let mut b = FieldBudget::new(it);
        let got: Vec<PropValue> = names
            .iter()
            .map(|n| decode(t, it, &prop(t, n), window(), &mut b))
            .collect();
        let text = PropValue::Text("שלום עולם".into());
        assert_eq!(&got[..4], &[text.clone(), text.clone(), text.clone(), text]);
        assert_eq!(&got[4..], &[PropValue::Skipped, PropValue::Skipped]);
        assert_eq!(b.skipped, 2);
    }

    #[test]
    fn variable_fields_in_both_heaps_decode() {
        let t = schema_for(0xc9).unwrap();
        let conv = vec![1, 0xe0, 2, 0xe0, 3];
        let v = make_typed_item(
            "MessageHeader",
            GUID,
            &[
                ("ConversationIndex", PropValue::Bytes(conv.clone())),
                ("Subject", PropValue::Text("subject".into())),
                ("SenderEmailAddress", PropValue::Text("a@b.example".into())),
            ],
        );
        let it = &item::scan(&v)[0];
        // ConversationIndex (dt 9) is the only heap A field.
        assert_eq!(it.heap_a_len(), conv.len());
        assert_eq!(get(t, it, "ConversationIndex"), PropValue::Bytes(conv));
        assert_eq!(get(t, it, "Subject"), PropValue::Text("subject".into()));
        assert_eq!(
            get(t, it, "SenderEmailAddress"),
            PropValue::Text("a@b.example".into())
        );
    }

    #[test]
    fn flagged_string_is_not_read_from_the_heap_start() {
        // Regression: with a non-empty heap A, a flagged descriptor at offset
        // 0 names the start of heap B, not the start of the heap.
        let t = schema_for(0xc9).unwrap();
        let v = make_typed_item(
            "MessageHeader",
            GUID,
            &[
                (
                    "ConversationIndex",
                    PropValue::Bytes(vec![0x41, 0, 0x42, 0]),
                ),
                ("ItemClass", PropValue::Text("IPM.Note".into())),
            ],
        );
        let it = &item::scan(&v)[0];
        let at = prop(t, "ItemClass").offset as usize;
        assert_eq!(&it.bytes[at..at + 4], &0u32.to_le_bytes());
        assert_eq!(get(t, it, "ItemClass"), PropValue::Text("IPM.Note".into()));
    }

    #[test]
    fn bad_heap_a_len_and_descriptors_are_missing() {
        let t = schema_for(0xc9).unwrap();
        let base = || {
            make_typed_item(
                "MessageHeader",
                GUID,
                &[
                    (
                        "ConversationIndex",
                        PropValue::Bytes(vec![1, 0xe0, 2, 0xe0]),
                    ),
                    ("Subject", PropValue::Text("s".into())),
                ],
            )
        };
        // A_len larger than the heap: heap B is empty.
        let mut v = base();
        v[4 + 0x68..4 + 0x6c].copy_from_slice(&u32::MAX.to_le_bytes());
        let it = &item::scan(&v)[0];
        assert_eq!(get(t, it, "Subject"), PropValue::Missing);
        assert_eq!(
            get(t, it, "ConversationIndex"),
            PropValue::Bytes(vec![1, 0xe0, 2, 0xe0])
        );
        // Unflagged descriptor one byte past heap A.
        let mut v = base();
        let at = 4 + prop(t, "ConversationIndex").offset as usize + 4;
        v[at..at + 4].copy_from_slice(&5u32.to_le_bytes());
        let it = &item::scan(&v)[0];
        assert_eq!(get(t, it, "ConversationIndex"), PropValue::Missing);
        // Flagged descriptor one byte past heap B.
        let mut v = base();
        let at = 4 + prop(t, "Subject").offset as usize + 4;
        v[at..at + 4].copy_from_slice(&(3u32 | 0x8000_0000).to_le_bytes());
        let it = &item::scan(&v)[0];
        assert_eq!(get(t, it, "Subject"), PropValue::Missing);
        // fixed_len < 0x6c: every variable field is out of the fixed part.
        let v = base();
        let full = &item::scan(&v)[0];
        let it = Item {
            fixed_len: 0x60,
            ..full.clone()
        };
        assert_eq!(it.heap_a_len(), 0);
        assert_eq!(get(t, &it, "Subject"), PropValue::Missing);
        assert_eq!(get(t, &it, "ConversationIndex"), PropValue::Missing);
    }

    #[test]
    fn guid_helpers_and_unknown_type() {
        let v = make_typed_item("MessageHeader", [0; 16], &[]);
        let it = &item::scan(&v)[0];
        assert_eq!(guid_of(it), None);
        let short = Item {
            bytes: &it.bytes[..20],
            ..it.clone()
        };
        assert_eq!(guid_of(&short), None);
        assert_eq!(guid_hex(&GUID), "0123456789abcdeffedcba9876543210");
        assert_eq!(guid_hex(&[0; 16]), "0".repeat(32));
        assert_eq!(schema_for(0xd7), None);
        assert_eq!(schema_for(0xffff), None);
        assert_eq!(schema_for(0xc9).map(|t| t.name), Some("MessageHeader"));
        let mh = schema_for(0xc9).unwrap();
        assert_eq!(column_name(&prop(mh, "Subject")), "Subject");
        let hidden = mh.props.iter().find(|p| p.name.is_empty()).unwrap();
        assert!(column_name(hidden).starts_with("prop_") || column_name(hidden).starts_with("ci_"));
    }
}
