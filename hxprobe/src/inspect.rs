//! Inspection over the structural layer: a histogram of object types and a
//! per-object dump: named, typed properties for items a schema applies to
//! (see [`crate::props`]), discovered fields for every other item.
//!
//! Both walk the live page values of a [`Store`] one at a time (decompressed
//! on demand, then dropped) and find objects with the item envelope scan, so
//! stale revisions are never counted and memory stays near one value.

use std::collections::BTreeMap;

use crate::item::{self, FieldBudget, Item, Text};
use crate::kv::Store;
use crate::names;
use crate::props::{self, PropValue};

/// Longest text shown per field, in chars.
pub const TEXT_LIMIT: usize = 80;

/// Whether a type's schema applies to its items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaStatus {
    /// A schema exists and every observed `fixed_len` equals its own.
    Yes,
    /// A schema exists and at least one observed `fixed_len` differs.
    SizeDiffers,
    /// No schema for the type.
    No,
}

impl SchemaStatus {
    /// Of a type with `schema`, whose items show `fixed_lens`.
    pub fn of(schema: Option<&props::TypeSchema>, fixed_lens: &[usize]) -> SchemaStatus {
        match schema {
            None => SchemaStatus::No,
            Some(t) if fixed_lens.iter().all(|&l| l == t.fixed_len as usize) => SchemaStatus::Yes,
            Some(_) => SchemaStatus::SizeDiffers,
        }
    }

    /// Text shown by `types`.
    pub fn as_str(self) -> &'static str {
        match self {
            SchemaStatus::Yes => "yes",
            SchemaStatus::SizeDiffers => "size differs",
            SchemaStatus::No => "no",
        }
    }
}

/// One row of the type histogram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRow {
    pub id: u16,
    pub name: Option<&'static str>,
    /// Items found across all live values.
    pub count: usize,
    /// Distinct `fixed_len` values seen, ascending (one for a well-formed type).
    pub fixed_lens: Vec<usize>,
    /// Whether the type's schema applies to the items seen.
    pub schema: SchemaStatus,
}

/// Result of [`type_histogram`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Histogram {
    /// Sorted by type id.
    pub rows: Vec<TypeRow>,
    /// Item envelopes left out by the scan budget (`item::scan_budgeted`).
    pub skipped_items: usize,
}

/// Result of [`dump_objects`].
#[derive(Debug, Clone, PartialEq)]
pub struct Dump {
    pub objects: Vec<ObjectDump>,
    /// Item envelopes left out by the scan budget in the values read.
    pub skipped_items: usize,
}

/// How a dumped generic field decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    Utf16(String),
    Utf8(String),
    Binary,
    /// Unix seconds of a tick field.
    Ticks(i64),
    /// A valid descriptor left undecoded: the item's `item::FieldBudget` was
    /// spent.
    Skipped,
}

impl Decoded {
    /// Short name of the decoded kind.
    pub fn kind(&self) -> &'static str {
        match self {
            Decoded::Utf16(_) => "utf16",
            Decoded::Utf8(_) => "utf8",
            Decoded::Binary => "binary",
            Decoded::Ticks(_) => "ticks",
            Decoded::Skipped => "skipped",
        }
    }
}

/// One field of a dumped object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDump {
    /// Item offset of the descriptor or tick.
    pub at: usize,
    /// Heap bytes named by a descriptor, 8 for ticks, 0 when missing.
    pub len: usize,
    /// Text is truncated to [`TEXT_LIMIT`] chars.
    pub value: Decoded,
    /// True when the text was cut at [`TEXT_LIMIT`].
    pub truncated: bool,
}

/// One property of a dumped schema object.
#[derive(Debug, Clone, PartialEq)]
pub struct PropDump {
    /// Column name of the property (`props::column_name`).
    pub name: String,
    /// The full decoded value (never `PropValue::Missing`).
    pub value: PropValue,
}

/// One dumped object.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectDump {
    /// Page key of the live value holding the object.
    pub page: u64,
    /// Offset of the item start within that value.
    pub offset: usize,
    pub type_id: u16,
    pub fixed_len: usize,
    pub size: usize,
    /// True when the item's type has a schema with its `fixed_len`
    /// (`props::applies`): `props` is filled and `fields` is empty.
    pub schema: bool,
    /// Every property whose value is not `Missing`, in ci order (schema
    /// objects only).
    pub props: Vec<PropDump>,
    /// Discovered fields, sorted by offset (generic objects only).
    pub fields: Vec<FieldDump>,
}

/// Call `f` with every item of every live page value, in key order. Stops
/// early when `f` returns `false`. Returns the number of item envelopes the
/// scan budget left out in the values read.
fn for_each_item(store: &Store, mm: &[u8], mut f: impl FnMut(u64, &Item) -> bool) -> usize {
    let mut skipped = 0usize;
    for &key in store.pages.keys() {
        let Some(data) = store.page_value(mm, key) else {
            continue;
        };
        let scan = item::scan_budgeted(&data);
        skipped += scan.skipped;
        for it in &scan.items {
            if !f(key, it) {
                return skipped;
            }
        }
    }
    skipped
}

/// Object type histogram over all live page values, sorted by type id.
pub fn type_histogram(store: &Store, mm: &[u8]) -> Histogram {
    let mut rows: BTreeMap<u16, TypeRow> = BTreeMap::new();
    let skipped_items = for_each_item(store, mm, |_, it| {
        let row = rows.entry(it.type_id).or_insert_with(|| TypeRow {
            id: it.type_id,
            name: names::type_name(it.type_id),
            count: 0,
            fixed_lens: Vec::new(),
            schema: SchemaStatus::No,
        });
        row.count += 1;
        if let Err(i) = row.fixed_lens.binary_search(&it.fixed_len) {
            row.fixed_lens.insert(i, it.fixed_len);
        }
        true
    });
    let mut rows: Vec<TypeRow> = rows.into_values().collect();
    for r in &mut rows {
        r.schema = SchemaStatus::of(props::schema_for(r.id), &r.fixed_lens);
    }
    Histogram {
        rows,
        skipped_items,
    }
}

/// `s` cut to [`TEXT_LIMIT`] chars, and whether anything was cut.
fn cut_text(s: &str) -> (String, bool) {
    let truncated = s.chars().count() > TEXT_LIMIT;
    (s.chars().take(TEXT_LIMIT).collect(), truncated)
}

fn text_field(at: usize, data: &[u8]) -> FieldDump {
    let (value, truncated) = match item::decode_text(data) {
        Text::Utf16(s) => {
            let (s, t) = cut_text(&s);
            (Decoded::Utf16(s), t)
        }
        Text::Utf8(s) => {
            let (s, t) = cut_text(&s);
            (Decoded::Utf8(s), t)
        }
        Text::Binary => (Decoded::Binary, false),
    };
    FieldDump {
        at,
        len: data.len(),
        value,
        truncated,
    }
}

fn plain(at: usize, len: usize, value: Decoded) -> FieldDump {
    FieldDump {
        at,
        len,
        value,
        truncated: false,
    }
}

/// Properties of `it` per its schema: every property not `Missing`, in ci
/// order, decoded with one [`FieldBudget`] for the item.
fn schema_props(t: &props::TypeSchema, it: &Item, window: (u64, u64)) -> Vec<PropDump> {
    let mut budget = FieldBudget::new(it);
    t.props
        .iter()
        .filter_map(|p| match props::decode(t, it, p, window, &mut budget) {
            PropValue::Missing => None,
            value => Some(PropDump {
                name: props::column_name(p),
                value,
            }),
        })
        .collect()
}

/// Fields of `it` without a usable schema: every discovered descriptor, plus a tick
/// at every byte offset from 16 to `fixed_len - 8` inside the tick window.
fn generic_fields(it: &Item, (lo, hi): (u64, u64)) -> Vec<FieldDump> {
    let mut budget = FieldBudget::new(it);
    let mut out: Vec<FieldDump> = it
        .descriptors()
        .into_iter()
        .map(|v| {
            if budget.take(v.data.len()) {
                text_field(v.at, v.data)
            } else {
                plain(v.at, v.data.len(), Decoded::Skipped)
            }
        })
        .collect();
    for at in 16..=it.fixed_len.saturating_sub(8) {
        let t = u64::from_le_bytes(it.bytes[at..at + 8].try_into().unwrap());
        if (lo..=hi).contains(&t) {
            out.push(plain(at, 8, Decoded::Ticks(item::ticks_to_unix(t))));
        }
    }
    out.sort_by_key(|f| f.at);
    out
}

/// The first `n` objects of `type_id` in live page values, in key order.
/// `window` is the tick window of the run (`item::tick_window`).
pub fn dump_objects(store: &Store, mm: &[u8], type_id: u16, n: usize, window: (u64, u64)) -> Dump {
    let mut out = Vec::new();
    if n == 0 {
        return Dump {
            objects: out,
            skipped_items: 0,
        };
    }
    let schema = props::schema_for(type_id);
    let skipped_items = for_each_item(store, mm, |page, it| {
        if it.type_id == type_id {
            let t = schema.filter(|t| props::applies(t, it));
            let (props, fields) = match t {
                Some(t) => (schema_props(t, it, window), Vec::new()),
                None => (Vec::new(), generic_fields(it, window)),
            };
            out.push(ObjectDump {
                page,
                offset: it.offset,
                type_id,
                fixed_len: it.fixed_len,
                size: it.bytes.len(),
                schema: t.is_some(),
                props,
                fields,
            });
        }
        out.len() < n
    });
    Dump {
        objects: out,
        skipped_items,
    }
}

/// Proleptic Gregorian `(year, month, day)` of `days` since 1970-01-01
/// (Howard Hinnant's `civil_from_days`).
pub fn days_to_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `YYYY-MM-DDThh:mm:ssZ` of unix seconds `t`.
pub fn utc_string(t: i64) -> String {
    let (y, mo, d) = days_to_date(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// `kind value` of a dumped property: text cut to [`TEXT_LIMIT`] chars
/// (`...` when cut), bytes as their length, references as
/// `TypeName:guidhex`, GUIDs as hex, times as unix seconds plus UTC.
pub fn prop_text(v: &PropValue) -> String {
    match v {
        PropValue::Missing => "missing".into(),
        PropValue::Skipped => "skipped".into(),
        PropValue::Bool(b) => format!("bool {b}"),
        PropValue::Int(i) => format!("int {i}"),
        PropValue::Real(f) => format!("real {f}"),
        PropValue::Time(t) => format!("time {t} {}", utc_string(*t)),
        PropValue::Text(s) => {
            let (cut, truncated) = cut_text(s);
            let more = if truncated { "..." } else { "" };
            format!("text {cut:?}{more}")
        }
        PropValue::Bytes(b) => format!("bytes {}", b.len()),
        PropValue::Guid(g) => format!("guid {}", props::guid_hex(g)),
        PropValue::Ref { type_id, guid } => {
            let name =
                names::type_name(*type_id).map_or_else(|| format!("0x{type_id:x}"), String::from);
            format!("ref {name}:{}", props::guid_hex(guid))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        copy_descriptor, make_block, make_item, make_node_pack, make_store, make_typed_item,
        overlapping_envelopes, page, recipient, set_fixed_len, utf16, window, T2024,
    };

    fn person() -> Vec<u8> {
        make_item(0x55, 0x15e, &[(0x124, &utf16("P One"))], &[])
    }

    fn dump(s: &Store, mm: &[u8], type_id: u16, n: usize) -> Vec<ObjectDump> {
        dump_objects(s, mm, type_id, n, window()).objects
    }

    #[test]
    fn histogram_counts_two_types_and_names_them() {
        let a = [recipient("Ann Lee", "ann@example.org"), person()].concat();
        let b = [
            recipient("Bob Roe", "bob@example.org"),
            recipient("C D", "c@d.e"),
        ]
        .concat();
        let mm = make_store(&[
            make_block(&page(1), &make_node_pack(&a, 2, &[])),
            make_block(&page(2), &b),
        ]);
        let s = Store::load(&mm);
        let h = type_histogram(&s, &mm);
        assert_eq!(h.skipped_items, 0);
        assert_eq!(
            h.rows,
            vec![
                TypeRow {
                    id: 0x55,
                    name: Some("Person"),
                    count: 1,
                    fixed_lens: vec![0x15e],
                    schema: SchemaStatus::Yes,
                },
                TypeRow {
                    id: 0xd2,
                    name: Some("Recipient"),
                    count: 3,
                    fixed_lens: vec![0x15a],
                    schema: SchemaStatus::Yes,
                },
            ]
        );
    }

    #[test]
    fn histogram_reports_schema_status() {
        let mut short = recipient("Ann Lee", "ann@example.org");
        set_fixed_len(&mut short, 0x150);
        let v = [
            // Person: schema and its fixed_len only.
            person(),
            // Recipient: one item with the schema's fixed_len, one without.
            recipient("Bob Roe", "bob@example.org"),
            short,
            // 0x56: no schema.
            make_item(0x56, 0x40, &[(0x10, b"no schema")], &[]),
        ]
        .concat();
        let mm = make_store(&[make_block(&page(1), &v)]);
        let s = Store::load(&mm);
        let got: Vec<(u16, SchemaStatus)> = type_histogram(&s, &mm)
            .rows
            .iter()
            .map(|r| (r.id, r.schema))
            .collect();
        assert_eq!(
            got,
            [
                (0x55, SchemaStatus::Yes),
                (0x56, SchemaStatus::No),
                (0xd2, SchemaStatus::SizeDiffers),
            ]
        );
        assert!(props::schema_for(0x56).is_none());
        let text: Vec<&str> = got.iter().map(|(_, st)| st.as_str()).collect();
        assert_eq!(text, ["yes", "no", "size differs"]);
    }

    #[test]
    fn histogram_skips_stale_revisions() {
        let old = [recipient("Old One", "o@example.org"), person(), person()].concat();
        let mm = make_store(&[
            make_block(&page(7), &old),
            make_block(&page(7), &recipient("New One", "n@example.org")),
        ]);
        let s = Store::load(&mm);
        assert_eq!(s.stats.stale_revisions, 1);
        let rows = type_histogram(&s, &mm).rows;
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].id, rows[0].count), (0xd2, 1));
    }

    fn typed_recipient(guid: [u8; 16], name: &str, email: &str) -> Vec<u8> {
        make_typed_item(
            "Recipient",
            guid,
            &[
                ("DisplayName", PropValue::Text(name.into())),
                ("EmailAddress", PropValue::Text(email.into())),
            ],
        )
    }

    /// `(name, value)` of every dumped property.
    fn props_of(o: &ObjectDump) -> Vec<(&str, &PropValue)> {
        o.props
            .iter()
            .map(|p| (p.name.as_str(), &p.value))
            .collect()
    }

    #[test]
    fn dump_recipient_lists_schema_properties() {
        let v = [
            typed_recipient([1; 16], "דנה כהן", "dana@example.org"),
            typed_recipient([2; 16], "Bob Roe", "b@y.z"),
        ]
        .concat();
        let mm = make_store(&[make_block(&page(3), &v)]);
        let s = Store::load(&mm);
        let d = dump(&s, &mm, 0xd2, 1);
        assert_eq!(d.len(), 1);
        let o = &d[0];
        assert!(o.schema);
        assert!(o.fields.is_empty());
        assert_eq!((o.page, o.offset, o.fixed_len), (3, 4, 0x15a));
        let dana = PropValue::Text("דנה כהן".into());
        let email = PropValue::Text("dana@example.org".into());
        let got = props_of(o);
        assert_eq!(got[..2], [("DisplayName", &dana), ("EmailAddress", &email)]);
        // Zero scalars are values, not Missing; unset strings are left out.
        assert!(got.contains(&("SubType", &PropValue::Int(0))));
        assert!(!got.iter().any(|(n, _)| *n == "GivenName"));
        assert_eq!(dump(&s, &mm, 0xd2, 10).len(), 2);
    }

    #[test]
    fn dump_message_header_lists_typed_values() {
        let gv = [0x4d; 16];
        let h = make_typed_item(
            "MessageHeader",
            [7; 16],
            &[
                ("Subject", PropValue::Text("Hello".into())),
                ("IsRead", PropValue::Bool(true)),
                ("SentTime", PropValue::Time(1_709_251_200)),
                (
                    "View",
                    PropValue::Ref {
                        type_id: 0x4d,
                        guid: gv,
                    },
                ),
            ],
        );
        let mm = make_store(&[make_block(&page(1), &h)]);
        let s = Store::load(&mm);
        let d = dump(&s, &mm, 0xc9, 1);
        assert!(d[0].schema);
        let got = props_of(&d[0]);
        let find = |n: &str| got.iter().find(|(k, _)| *k == n).map(|(_, v)| *v);
        assert_eq!(find("Subject"), Some(&PropValue::Text("Hello".into())));
        assert_eq!(find("IsRead"), Some(&PropValue::Bool(true)));
        assert_eq!(find("SentTime"), Some(&PropValue::Time(1_709_251_200)));
        assert_eq!(
            find("View"),
            Some(&PropValue::Ref {
                type_id: 0x4d,
                guid: gv
            })
        );
        // Clear booleans are listed (they are not Missing); unset strings
        // are not.
        assert_eq!(find("IsDraft"), Some(&PropValue::Bool(false)));
        assert_eq!(find("Preview"), None);
        assert!(got.iter().all(|(_, v)| **v != PropValue::Missing));
        // Listed in ci order.
        let t = props::schema_for(0xc9).unwrap();
        let ci = |n: &str| {
            let p = t.props.iter().find(|p| props::column_name(p) == n);
            p.unwrap().ci
        };
        let cis: Vec<u16> = got.iter().map(|(n, _)| ci(n)).collect();
        assert!(cis.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn dump_marks_schema_fields_past_the_budget_as_skipped() {
        // Six string properties all name the Subject's heap bytes: four fit.
        let mut v = make_typed_item(
            "MessageHeader",
            [7; 16],
            &[("Subject", PropValue::Text("same bytes".into()))],
        );
        let t = props::schema_for(0xc9).unwrap();
        let at = |name: &str| t.props.iter().find(|p| p.name == name).unwrap().offset as usize;
        for name in [
            "Preview",
            "NormalizedSubject",
            "SenderDisplayName",
            "SenderEmailAddress",
            "InternetMessageId",
        ] {
            copy_descriptor(&mut v, at("Subject"), at(name));
        }
        let mm = make_store(&[make_block(&page(1), &v)]);
        let s = Store::load(&mm);
        let d = dump(&s, &mm, 0xc9, 1);
        let texts = d[0]
            .props
            .iter()
            .filter(|p| matches!(p.value, PropValue::Text(_)))
            .count();
        let skipped = d[0]
            .props
            .iter()
            .filter(|p| p.value == PropValue::Skipped)
            .count();
        assert_eq!((texts, skipped), (4, 2));
    }

    #[test]
    fn dump_with_wrong_fixed_len_is_generic() {
        let mut v = typed_recipient([1; 16], "Ann Lee", "ann@example.org");
        let fl = u16::from_le_bytes([v[6], v[7]]) - 2;
        set_fixed_len(&mut v, fl);
        let mm = make_store(&[make_block(&page(1), &v)]);
        let s = Store::load(&mm);
        let d = dump(&s, &mm, 0xd2, 1);
        assert!(!d[0].schema);
        assert!(d[0].props.is_empty());
    }

    #[test]
    fn days_to_date_known_days() {
        assert_eq!(days_to_date(0), (1970, 1, 1));
        assert_eq!(days_to_date(11_016), (2000, 2, 29));
        assert_eq!(days_to_date(20_779), (2026, 11, 22));
        assert_eq!(days_to_date(-1), (1969, 12, 31));
        assert_eq!(utc_string(1_709_251_200 + 3_723), "2024-03-01T01:02:03Z");
    }

    #[test]
    fn prop_text_formats_each_kind() {
        let g = [0xab; 16];
        let hex = "ab".repeat(16);
        assert_eq!(prop_text(&PropValue::Bool(true)), "bool true");
        assert_eq!(prop_text(&PropValue::Int(-3)), "int -3");
        assert_eq!(prop_text(&PropValue::Bytes(vec![0; 5])), "bytes 5");
        assert_eq!(prop_text(&PropValue::Guid(g)), format!("guid {hex}"));
        assert_eq!(
            prop_text(&PropValue::Ref {
                type_id: 0x4d,
                guid: g
            }),
            format!("ref View:{hex}")
        );
        assert_eq!(
            prop_text(&PropValue::Time(0)),
            "time 0 1970-01-01T00:00:00Z"
        );
        assert_eq!(prop_text(&PropValue::Skipped), "skipped");
        let long = "y".repeat(100);
        let want = format!("text {:?}...", "y".repeat(TEXT_LIMIT));
        assert_eq!(prop_text(&PropValue::Text(long)), want);
        assert_eq!(prop_text(&PropValue::Text("a".into())), "text \"a\"");
    }

    #[test]
    fn dump_without_schema_lists_descriptors_and_ticks() {
        let long = "x ".repeat(60);
        let it = make_item(
            0x56,
            0x40,
            &[(0x10, long.as_bytes()), (0x20, &[0x01, 0xe0, 0x02, 0xe0])],
            &[(0x30, T2024)],
        );
        let mm = make_store(&[make_block(&page(1), &it)]);
        let s = Store::load(&mm);
        let d = dump(&s, &mm, 0x56, 5);
        assert_eq!(d.len(), 1);
        assert!(!d[0].schema);
        let got: Vec<(usize, &str)> = d[0].fields.iter().map(|f| (f.at, f.value.kind())).collect();
        assert_eq!(got, [(0x10, "utf8"), (0x20, "binary"), (0x30, "ticks")]);
        let t = &d[0].fields[0];
        assert!(t.truncated);
        assert_eq!(t.len, 120);
        assert_eq!(
            t.value,
            Decoded::Utf8(long.chars().take(TEXT_LIMIT).collect())
        );
    }

    #[test]
    fn dump_of_absent_type_is_empty() {
        let mm = make_store(&[make_block(&page(1), &person())]);
        let s = Store::load(&mm);
        assert!(dump(&s, &mm, 0xd2, 5).is_empty());
        assert!(dump(&s, &mm, 0x55, 0).is_empty());
        let empty = make_store(&[]);
        assert!(type_histogram(&Store::load(&empty), &empty).rows.is_empty());
        assert_eq!(names::type_id("NoSuchType"), None);
    }

    #[test]
    fn scan_budget_is_reported_by_histogram_and_dump() {
        let len = 1 << 16;
        let mm = make_store(&[make_block(&page(1), &overlapping_envelopes(len))]);
        let s = Store::load(&mm);
        let h = type_histogram(&s, &mm);
        assert_eq!(h.rows.len(), 1);
        assert!(h.skipped_items > 0);
        assert_eq!(h.rows[0].count + h.skipped_items, len / 16 - 1);
        let d = dump_objects(&s, &mm, 0x56, usize::MAX, window());
        assert_eq!(d.skipped_items, h.skipped_items);
        assert_eq!(d.objects.len(), h.rows[0].count);
    }

    #[test]
    fn dump_marks_fields_past_the_field_budget_as_skipped() {
        // Six descriptors that all name the same 12 heap bytes: four fit.
        let mut v = make_item(0x56, 0x40, &[(0x10, b"hello world!")], &[]);
        let desc: [u8; 8] = v[4 + 0x10..4 + 0x18].try_into().unwrap();
        for at in [0x18, 0x20, 0x28, 0x30, 0x38] {
            v[4 + at..4 + at + 8].copy_from_slice(&desc);
        }
        let mm = make_store(&[make_block(&page(1), &v)]);
        let s = Store::load(&mm);
        let d = dump(&s, &mm, 0x56, 1);
        let kinds: Vec<&str> = d[0].fields.iter().map(|f| f.value.kind()).collect();
        assert_eq!(
            kinds,
            ["utf8", "utf8", "utf8", "utf8", "skipped", "skipped"]
        );
        assert_eq!(d[0].fields[5].len, 12);
    }
}
