//! Structural SQLite export: every object of every live value, its fields
//! and the collections of node packs.
//!
//! Live page values are decompressed one at a time and dropped, so memory
//! stays near one value. The export makes two passes over the live pages in
//! key order: [`objects::index`] picks one winning copy per schema object
//! `(type_id, guid)`, then the write pass exports each item.
//!
//! - Schema objects (type with a schema, the schema's `fixed_len`, non-zero
//!   GUID): the winning copy becomes one `objects` row (`guid` set,
//!   `has_schema = 1`) and one row of its type table, a table named exactly
//!   as the type with one column per property (see [`crate::props`]). The
//!   other copies are counted in `superseded`.
//! - Every other item takes the generic path: an `objects` row with `guid`
//!   NULL, deduplicated by content (the 64-bit FNV-1a hash of the item bytes
//!   together with its size and type; `objects.page` / `item_offset` name the
//!   first occurrence in key order), and one `fields` row per variable field
//!   found by descriptor discovery (`Item::descriptors`); `fields.label` is
//!   NULL.
//!
//! `u64` page ids and hashes are stored bit-cast to `i64`. References and
//! GUIDs are lowercase hex text, so joins are plain `ON a.View = b.guid`.
//!
//! Views over the type tables are always created: the curated `messages`,
//! `folders`, `recipients`, `people` ([`VIEWS`]) and the compatibility views
//! `message_headers`, `message_data`, `views` ([`COMPAT_VIEWS`]).
//!
//! Work per value is bounded: see `item::ITEM_BUDGET_FACTOR` and
//! `item::FIELD_BUDGET_FACTOR`. What a budget leaves out is counted in
//! [`ExportStats`], never dropped silently.

use std::collections::HashMap;

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, Statement};

use crate::item::{self, FieldBudget, Item, Text};
use crate::kv::Store;
use crate::names;
use crate::objects;
use crate::props::{self, Prop, PropValue, TypeSchema};
use crate::value::{self, Value};

/// Counters from one [`export`] run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExportStats {
    /// Live page values decompressed and scanned.
    pub values: usize,
    /// Rows written to `objects`: `schema_objects + generic_objects`.
    pub objects: usize,
    /// Schema objects written (one per `(type_id, guid)`).
    pub schema_objects: usize,
    /// Generic objects written (distinct items by content).
    pub generic_objects: usize,
    /// Generic items skipped because an identical item was already exported.
    pub duplicates: usize,
    /// Copies of a schema object that lost to a newer copy (not written).
    pub superseded: usize,
    /// Items of a schema type whose `fixed_len` differs from the schema
    /// (exported generically).
    pub fixed_len_mismatch: usize,
    /// Items of a schema type with an all-zero GUID (exported generically).
    pub no_guid: usize,
    /// Rows written to `fields`.
    pub fields: usize,
    /// Rows written to `collections`.
    pub collections: usize,
    /// Item envelopes left out by the scan budget (`item::scan_budgeted`),
    /// counted in the write pass only.
    pub skipped_items: usize,
    /// Variable fields left out by the per-item `item::FieldBudget`, on both
    /// paths.
    pub skipped_fields: usize,
}

/// Why an [`export`] failed.
#[derive(Debug)]
pub enum ExportError {
    /// SQLite refused a statement.
    Sql(rusqlite::Error),
    /// The index pass and the write pass saw different data.
    StoreChanged {
        /// `(schema_objects, skipped_items)` of the index pass.
        indexed: (usize, usize),
        /// `(schema_objects, skipped_items)` of the write pass.
        written: (usize, usize),
    },
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Sql(e) => e.fmt(f),
            ExportError::StoreChanged { indexed, written } => write!(
                f,
                "the store changed between the two passes (indexed {} schema objects and {} \
                 skipped items, wrote {} and {}); run the export on a snapshot copy of the store",
                indexed.0, indexed.1, written.0, written.1
            ),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<rusqlite::Error> for ExportError {
    fn from(e: rusqlite::Error) -> ExportError {
        ExportError::Sql(e)
    }
}

/// Check that the write pass saw what the index pass saw. They read the same
/// bytes, so a difference means the store was modified during the export.
///
/// Both arguments are `(schema_objects, skipped_items)`.
fn check_passes(indexed: (usize, usize), written: (usize, usize)) -> Result<(), ExportError> {
    if indexed == written {
        Ok(())
    } else {
        Err(ExportError::StoreChanged { indexed, written })
    }
}

/// Type tables created even when the store holds no object of the type
/// (the views in [`VIEWS`] and [`COMPAT_VIEWS`] read them).
pub const ALWAYS_TABLES: [&str; 5] = [
    "MessageHeader",
    "MessageData",
    "View",
    "Recipient",
    "Person",
];

const SCHEMA: &str = "
CREATE TABLE objects(id INTEGER PRIMARY KEY, type_id INTEGER, type_name TEXT, guid TEXT,
                     page INTEGER, item_offset INTEGER, fixed_len INTEGER, size INTEGER,
                     content_hash INTEGER, has_schema INTEGER);
CREATE UNIQUE INDEX objects_content ON objects(content_hash, size, type_id) WHERE guid IS NULL;
CREATE UNIQUE INDEX objects_guid ON objects(type_id, guid) WHERE guid IS NOT NULL;
CREATE TABLE fields(object_id INTEGER, at INTEGER, kind TEXT, label TEXT,
                    text TEXT, bytes BLOB, unix_time INTEGER);
CREATE TABLE collections(page INTEGER, type_id INTEGER, type_name TEXT,
                         owner_page INTEGER, owner_type_id INTEGER, owner_type_name TEXT,
                         size INTEGER);
";

/// One table of a curated view: `alias` names it in the view, `table` is a
/// type table, and `on` is `(alias, property)` of an earlier source whose
/// reference is joined (`LEFT JOIN`) to this table's `guid`; `None` for the
/// first source.
pub struct ViewSource {
    pub alias: &'static str,
    pub table: &'static str,
    pub on: Option<(&'static str, &'static str)>,
}

/// One column of a curated view: `column` of the source `alias`, named
/// `name` in the view.
pub struct ViewColumn {
    pub alias: &'static str,
    pub column: &'static str,
    pub name: &'static str,
}

/// A curated view over the type tables.
pub struct ViewDef {
    pub name: &'static str,
    pub sources: &'static [ViewSource],
    pub columns: &'static [ViewColumn],
}

const fn col(alias: &'static str, column: &'static str, name: &'static str) -> ViewColumn {
    ViewColumn {
        alias,
        column,
        name,
    }
}

/// Columns of `messages`: one row per message header with its folder name and
/// body.
pub const MESSAGES_COLUMNS: &[ViewColumn] = &[
    col("h", "guid", "guid"),
    col("h", "Subject", "Subject"),
    col("h", "SenderDisplayName", "SenderDisplayName"),
    col("h", "SenderEmailAddress", "SenderEmailAddress"),
    col("h", "SentTime", "SentTime"),
    col("h", "DisplayTime", "DisplayTime"),
    col("h", "IsRead", "IsRead"),
    col("h", "Importance", "Importance"),
    col("h", "HasFileAttachment", "HasFileAttachment"),
    col("h", "Preview", "Preview"),
    col("h", "InternetMessageId", "InternetMessageId"),
    col("v", "Name", "folder"),
    col("v", "guid", "folder_guid"),
    col("d", "HTMLBodyBytes", "body_html"),
    col("d", "HTMLFirstBodyBytes", "body_first"),
    col("d", "BodyEncoding", "body_encoding"),
];

/// Columns of `folders`: one row per View with its parent's name.
pub const FOLDERS_COLUMNS: &[ViewColumn] = &[
    col("v", "guid", "guid"),
    col("v", "Name", "Name"),
    col("v", "Type", "Type"),
    col("v", "TotalCount", "TotalCount"),
    col("v", "UnreadCount", "UnreadCount"),
    col("p", "Name", "parent"),
    col("p", "guid", "parent_guid"),
];

/// Columns of `recipients`.
pub const RECIPIENTS_COLUMNS: &[ViewColumn] = &[
    col("r", "object_id", "object_id"),
    col("r", "page", "page"),
    col("r", "guid", "guid"),
    col("r", "DisplayName", "display_name"),
    col("r", "EmailAddress", "email"),
    col("r", "GivenName", "GivenName"),
    col("r", "Surname", "Surname"),
];

/// Columns of `people`.
pub const PEOPLE_COLUMNS: &[ViewColumn] = &[
    col("p", "object_id", "object_id"),
    col("p", "page", "page"),
    col("p", "guid", "guid"),
    col("p", "DisplayName", "name"),
    col("p", "EmailAddress", "email"),
];

/// The curated views. Every table they read is in [`ALWAYS_TABLES`], so the
/// views always resolve.
pub const VIEWS: &[ViewDef] = &[
    ViewDef {
        name: "messages",
        sources: &[
            ViewSource {
                alias: "h",
                table: "MessageHeader",
                on: None,
            },
            ViewSource {
                alias: "v",
                table: "View",
                on: Some(("h", "View")),
            },
            ViewSource {
                alias: "d",
                table: "MessageData",
                on: Some(("h", "MessageData")),
            },
        ],
        columns: MESSAGES_COLUMNS,
    },
    ViewDef {
        name: "folders",
        sources: &[
            ViewSource {
                alias: "v",
                table: "View",
                on: None,
            },
            ViewSource {
                alias: "p",
                table: "View",
                on: Some(("v", "ParentView")),
            },
        ],
        columns: FOLDERS_COLUMNS,
    },
    ViewDef {
        name: "recipients",
        sources: &[ViewSource {
            alias: "r",
            table: "Recipient",
            on: None,
        }],
        columns: RECIPIENTS_COLUMNS,
    },
    ViewDef {
        name: "people",
        sources: &[ViewSource {
            alias: "p",
            table: "Person",
            on: None,
        }],
        columns: PEOPLE_COLUMNS,
    },
];

/// Compatibility views: `(view, type table)`, each `SELECT *` of the table.
pub const COMPAT_VIEWS: [(&str, &str); 3] = [
    ("message_headers", "MessageHeader"),
    ("message_data", "MessageData"),
    ("views", "View"),
];

/// `CREATE VIEW` of `v`. Every identifier goes through [`props::quote_ident`].
pub fn view_sql(v: &ViewDef) -> String {
    let q = props::quote_ident;
    let cols: Vec<String> = v
        .columns
        .iter()
        .map(|c| format!("{}.{} AS {}", q(c.alias), q(c.column), q(c.name)))
        .collect();
    let mut from = String::new();
    for s in v.sources {
        match s.on {
            None => from.push_str(&format!("{} {}", q(s.table), q(s.alias))),
            Some((left, prop)) => from.push_str(&format!(
                " LEFT JOIN {} {} ON {}.{} = {}.{}",
                q(s.table),
                q(s.alias),
                q(left),
                q(prop),
                q(s.alias),
                q("guid")
            )),
        }
    }
    format!(
        "CREATE VIEW {} AS SELECT {} FROM {from}",
        q(v.name),
        cols.join(", ")
    )
}

/// Create the curated and compatibility views.
fn create_views(conn: &Connection) -> rusqlite::Result<()> {
    for v in VIEWS {
        conn.execute_batch(&view_sql(v))?;
    }
    for (view, table) in COMPAT_VIEWS {
        conn.execute_batch(&format!(
            "CREATE VIEW {} AS SELECT * FROM {}",
            props::quote_ident(view),
            props::quote_ident(table)
        ))?;
    }
    Ok(())
}

/// Indexes built after the bulk insert (cheaper than maintaining them per row).
const INDEXES: &str = "
CREATE INDEX fields_object ON fields(object_id);
CREATE INDEX objects_type ON objects(type_id);
";

/// 64-bit FNV-1a.
pub fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// One `fields` row before insertion.
struct Row<'a> {
    at: usize,
    kind: &'static str,
    text: Option<String>,
    bytes: Option<&'a [u8]>,
}

/// A descriptor field decoded as text: the text when it decodes, otherwise
/// the raw bytes (never both, so a field is stored once).
fn text_row<'a>(at: usize, data: &'a [u8]) -> Row<'a> {
    let (kind, text, bytes) = match item::decode_text(data) {
        Text::Utf16(s) => ("utf16", Some(s), None),
        Text::Utf8(s) => ("utf8", Some(s), None),
        Text::Binary => ("binary", None, Some(data)),
    };
    Row {
        at,
        kind,
        text,
        bytes,
    }
}

/// Rows for `it`: every discovered descriptor. Variable fields past the
/// item's [`FieldBudget`] are left out and counted in `budget`.
fn rows<'a>(it: &Item<'a>, budget: &mut FieldBudget) -> Vec<Row<'a>> {
    it.descriptors()
        .into_iter()
        .filter(|v| budget.take(v.data.len()))
        .map(|v| text_row(v.at, v.data))
        .collect()
}

/// Declared SQL type of a property column (documentation for readers of the
/// database; values are bound as [`sql_value`] maps them).
fn column_type(p: &Prop) -> &'static str {
    match (p.offset, p.size, p.dt) {
        (o, ..) if o < 0 => "INTEGER",
        (_, 0, 7) => "TEXT",
        (_, 0, _) => "BLOB",
        (_, 1 | 2 | 4, _) => "INTEGER",
        (_, 8, 0x15) => "REAL",
        (_, 8, _) => "INTEGER",
        (_, 16 | 48, _) => "TEXT",
        _ => "BLOB",
    }
}

/// `CREATE TABLE` of the type table of `t`: `object_id`, `guid`, `page`, then
/// one column per property in ci order. Every identifier goes through
/// [`props::quote_ident`].
pub fn type_table_sql(t: &TypeSchema) -> String {
    let cols: String = t
        .props
        .iter()
        .map(|p| {
            format!(
                ",\n  {} {}",
                props::quote_ident(&props::column_name(p)),
                column_type(p)
            )
        })
        .collect();
    format!(
        "CREATE TABLE {}(object_id INTEGER PRIMARY KEY, guid TEXT UNIQUE, page INTEGER{cols})",
        props::quote_ident(t.name)
    )
}

/// The `INSERT` into the type table of `t`, one parameter per column.
fn type_insert_sql(t: &TypeSchema) -> String {
    let names: String = t
        .props
        .iter()
        .map(|p| format!(", {}", props::quote_ident(&props::column_name(p))))
        .collect();
    let marks = ", ?".repeat(t.props.len());
    format!(
        "INSERT INTO {} (object_id, guid, page{names}) VALUES (?, ?, ?{marks})",
        props::quote_ident(t.name)
    )
}

/// The column value of a decoded property; `Missing` and `Skipped` are NULL.
fn sql_value(v: PropValue) -> SqlValue {
    match v {
        PropValue::Missing | PropValue::Skipped => SqlValue::Null,
        PropValue::Bool(b) => SqlValue::Integer(i64::from(b)),
        PropValue::Int(i) | PropValue::Time(i) => SqlValue::Integer(i),
        PropValue::Real(f) => SqlValue::Real(f),
        PropValue::Text(s) => SqlValue::Text(s),
        PropValue::Bytes(b) => SqlValue::Blob(b),
        PropValue::Guid(g) | PropValue::Ref { guid: g, .. } => SqlValue::Text(props::guid_hex(&g)),
    }
}

/// The type tables of one export: created up front for [`ALWAYS_TABLES`],
/// else on the first object of the type, each with one prepared `INSERT`.
struct TypeTables<'c> {
    conn: &'c Connection,
    inserts: HashMap<u16, Statement<'c>>,
}

impl<'c> TypeTables<'c> {
    fn new(conn: &'c Connection) -> rusqlite::Result<TypeTables<'c>> {
        let mut tt = TypeTables {
            conn,
            inserts: HashMap::new(),
        };
        for name in ALWAYS_TABLES {
            let t = schema_named(name).expect("every always-present table has a schema");
            tt.insert_for(t)?;
        }
        Ok(tt)
    }

    /// The prepared `INSERT` of `t`, creating its table first if needed.
    fn insert_for(&mut self, t: &TypeSchema) -> rusqlite::Result<&mut Statement<'c>> {
        if !self.inserts.contains_key(&t.type_id) {
            self.conn.execute_batch(&type_table_sql(t))?;
            let st = self.conn.prepare(&type_insert_sql(t))?;
            self.inserts.insert(t.type_id, st);
        }
        Ok(self.inserts.get_mut(&t.type_id).expect("just inserted"))
    }
}

/// The schema of the type named `name`.
fn schema_named(name: &str) -> Option<&'static TypeSchema> {
    crate::props_gen::TYPES.iter().find(|t| t.name == name)
}

/// Export `store` into `conn` (which should be empty) in one transaction.
/// `window` is the tick window of the run (`item::tick_window`).
///
/// Fails with [`ExportError::StoreChanged`], leaving `conn` empty, when the
/// two passes over `mm` disagree.
pub fn export(
    store: &Store,
    mm: &[u8],
    conn: &Connection,
    window: (u64, u64),
) -> Result<ExportStats, ExportError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(SCHEMA)?;
    let ix = objects::index(store, mm);
    let mut st = ExportStats {
        superseded: ix.superseded,
        fixed_len_mismatch: ix.fixed_len_mismatch,
        no_guid: ix.no_guid,
        ..ExportStats::default()
    };
    {
        let mut tables = TypeTables::new(&tx)?;
        let mut ins_generic = tx.prepare(
            "INSERT OR IGNORE INTO objects
             (type_id, type_name, page, item_offset, fixed_len, size, content_hash, has_schema)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
        )?;
        // No OR IGNORE: the winner is unique per (type_id, guid), so a
        // conflict on `objects_guid` is a bug and must surface.
        let mut ins_schema = tx.prepare(
            "INSERT INTO objects
             (type_id, type_name, guid, page, item_offset, fixed_len, size, content_hash,
              has_schema)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
        )?;
        let mut ins_field = tx.prepare(
            "INSERT INTO fields (object_id, at, kind, text, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut ins_coll = tx.prepare(
            "INSERT INTO collections
             (page, type_id, type_name, owner_page, owner_type_id, owner_type_name, size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;

        for &key in store.pages.keys() {
            let Some(data) = store.page_value(mm, key) else {
                continue;
            };
            st.values += 1;

            if let Value::NodePack { collections, .. } = value::classify(key, &data) {
                for c in &collections {
                    // The entry's own page id sits at +4 (after version and 0x570).
                    let page = u64::from_le_bytes(c.bytes[4..12].try_into().unwrap());
                    ins_coll.execute(params![
                        page as i64,
                        c.type_id,
                        names::type_name(c.type_id),
                        c.owner.page as i64,
                        c.owner.type_id,
                        names::type_name(c.owner.type_id),
                        c.bytes.len() as i64,
                    ])?;
                    st.collections += 1;
                }
            }

            let scan = item::scan_budgeted(&data);
            st.skipped_items += scan.skipped;
            for it in &scan.items {
                let identity = props::schema_for(it.type_id)
                    .filter(|t| props::applies(t, it))
                    .and_then(|t| props::guid_of(it).map(|g| (t, g)));
                if let Some((t, guid)) = identity {
                    // Losing copies were counted by the index (`superseded`).
                    if !ix.is_winner(it.type_id, guid, key, it.offset) {
                        continue;
                    }
                    let guid = props::guid_hex(&guid);
                    ins_schema.execute(params![
                        it.type_id,
                        t.name,
                        guid,
                        key as i64,
                        it.offset as i64,
                        it.fixed_len as i64,
                        it.bytes.len() as i64,
                        fnv1a(it.bytes) as i64,
                    ])?;
                    let id = tx.last_insert_rowid();
                    let mut budget = FieldBudget::new(it);
                    let mut vals = Vec::with_capacity(t.props.len() + 3);
                    vals.push(SqlValue::Integer(id));
                    vals.push(SqlValue::Text(guid));
                    vals.push(SqlValue::Integer(key as i64));
                    for p in t.props {
                        vals.push(sql_value(props::decode(t, it, p, window, &mut budget)));
                    }
                    tables.insert_for(t)?.execute(params_from_iter(vals))?;
                    st.schema_objects += 1;
                    st.skipped_fields += budget.skipped;
                    continue;
                }

                let changed = ins_generic.execute(params![
                    it.type_id,
                    names::type_name(it.type_id),
                    key as i64,
                    it.offset as i64,
                    it.fixed_len as i64,
                    it.bytes.len() as i64,
                    fnv1a(it.bytes) as i64,
                ])?;
                if changed == 0 {
                    st.duplicates += 1;
                    continue;
                }
                st.generic_objects += 1;
                let id = tx.last_insert_rowid();
                let mut budget = FieldBudget::new(it);
                for r in rows(it, &mut budget) {
                    ins_field.execute(params![id, r.at as i64, r.kind, r.text, r.bytes])?;
                    st.fields += 1;
                }
                st.skipped_fields += budget.skipped;
            }
        }
    }
    check_passes(
        (ix.schema_objects, ix.skipped_items),
        (st.schema_objects, st.skipped_items),
    )?;
    st.objects = st.schema_objects + st.generic_objects;
    create_views(&tx)?;
    tx.execute_batch(INDEXES)?;
    tx.commit()?;
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::PropValue;
    use crate::testutil::{
        copy_descriptor, make_block, make_collection_entry, make_item, make_node_pack, make_store,
        make_typed_item, overlapping_envelopes, page, recipient, set_fixed_len, utf16, window,
    };
    use crate::value::ObjRef;

    fn run(mm: &[u8]) -> (Connection, ExportStats) {
        let conn = Connection::open_in_memory().unwrap();
        let st = export(&Store::load(mm), mm, &conn, window()).unwrap();
        (conn, st)
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn fnv1a_known_values() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    /// `(object id, at, kind, text)` of every `fields` row, in id and offset
    /// order.
    fn field_rows(conn: &Connection) -> Vec<(i64, i64, String, Option<String>)> {
        let mut q = conn
            .prepare("SELECT object_id, at, kind, text FROM fields ORDER BY object_id, at")
            .unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn duplicates_are_skipped_and_fields_pair_with_their_object() {
        let a = recipient("דנה כהן", "dana@example.org");
        let b = recipient("Bob Roe", "bob@y.z");
        // `a` again in another value: an exact duplicate.
        let mm = make_store(&[
            make_block(&page(1), &[a.clone(), b].concat()),
            make_block(&page(2), &a),
        ]);
        let (conn, st) = run(&mm);
        assert_eq!(st.values, 2);
        assert_eq!((st.objects, st.duplicates), (2, 1));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM objects"), 2);
        // Per recipient: the two discovered descriptors (name, email).
        assert_eq!(st.fields, 4);
        let t = |s: &str| Some(s.to_string());
        assert_eq!(
            field_rows(&conn),
            vec![
                (1, 0x100, "utf16".into(), t("דנה כהן")),
                (1, 0x108, "utf16".into(), t("dana@example.org")),
                (2, 0x100, "utf16".into(), t("Bob Roe")),
                (2, 0x108, "utf16".into(), t("bob@y.z")),
            ]
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM fields WHERE label IS NOT NULL"),
            0
        );
    }

    #[test]
    fn dedup_key_is_hash_size_and_type() {
        let (conn, _) = run(&make_store(&[]));
        let ins = |size: i64, type_id: i64| {
            conn.execute(
                "INSERT OR IGNORE INTO objects (type_id, size, content_hash) VALUES (?1, ?2, 7)",
                params![type_id, size],
            )
            .unwrap()
        };
        assert_eq!(ins(100, 0xd2), 1);
        // The same hash with another size or type is a different object.
        assert_eq!(ins(101, 0xd2), 1);
        assert_eq!(ins(100, 0x55), 1);
        // The same triple is a duplicate.
        assert_eq!(ins(100, 0xd2), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM objects"), 3);
    }

    #[test]
    fn duplicate_keeps_the_first_page() {
        let a = recipient("Ann Lee", "ann@example.org");
        let b = recipient("Bob Roe", "b@y.z");
        let mm = make_store(&[
            make_block(&page(9), &a),
            make_block(&page(4), &[b.clone(), a].concat()),
        ]);
        let (conn, st) = run(&mm);
        assert_eq!((st.objects, st.duplicates), (2, 1));
        // Key order, not file order: page 4 is read before page 9.
        let got: (i64, i64) = conn
            .query_row(
                "SELECT o.page, o.item_offset FROM fields f JOIN objects o ON o.id = f.object_id
                 WHERE f.text = 'ann@example.org'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(got, (4, b.len() as i64 + 4));
    }

    #[test]
    fn overlapping_envelopes_export_within_the_item_budget() {
        // 1 MiB of envelopes that each claim the rest of the value: about
        // 32 GiB of item bytes without the budget.
        let len = 1 << 20;
        let mm = make_store(&[make_block(&page(1), &overlapping_envelopes(len))]);
        let (conn, st) = run(&mm);
        assert_eq!(st.objects + st.duplicates + st.skipped_items, len / 16 - 1);
        assert!(st.skipped_items > len / 16 - 64, "{}", st.skipped_items);
        let bytes = count(&conn, "SELECT SUM(size) FROM objects");
        assert!(bytes <= (len * item::ITEM_BUDGET_FACTOR) as i64, "{bytes}");
    }

    #[test]
    fn descriptors_naming_one_heap_export_within_the_field_budget() {
        // Every 8-byte slot of the fixed part names the whole 4 KiB heap.
        let heap = [0x01u8, 0xe0].repeat(2048);
        let fixed_len = 0x4000;
        let mut v = make_item(0x56, fixed_len, &[(0x10, &heap)], &[]);
        let desc: [u8; 8] = v[4 + 0x10..4 + 0x18].try_into().unwrap();
        for at in (0x18..fixed_len).step_by(8) {
            v[4 + at..4 + at + 8].copy_from_slice(&desc);
        }
        let mm = make_store(&[make_block(&page(1), &v)]);
        let (conn, st) = run(&mm);
        let slots = (fixed_len - 0x10) / 8;
        assert_eq!(st.fields, item::FIELD_BUDGET_FACTOR);
        assert_eq!(st.fields + st.skipped_fields, slots);
        assert_eq!(
            count(&conn, "SELECT SUM(LENGTH(bytes)) FROM fields"),
            (heap.len() * item::FIELD_BUDGET_FACTOR) as i64
        );
    }

    #[test]
    fn binary_text_keeps_bytes() {
        // A descriptor whose data decodes as neither UTF-8 nor UTF-16.
        let bin = [0x01, 0xe0, 0x02, 0xe0];
        let r = make_item(0x56, 0x15a, &[(0x100, &bin)], &[]);
        let mm = make_store(&[make_block(&page(1), &r)]);
        let (conn, st) = run(&mm);
        assert_eq!((st.objects, st.fields), (1, 1));
        let (kind, text, bytes): (String, Option<String>, Vec<u8>) = conn
            .query_row(
                "SELECT kind, text, bytes FROM fields WHERE at = 256",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), text), ("binary", None));
        assert_eq!(bytes, bin);
    }

    #[test]
    fn collections_carry_owner_type_names() {
        let owner = ObjRef {
            prefix: 0,
            page: 0x99,
            type_id: 0xc9,
        };
        let e = make_collection_entry(0xd2, 0x1234, owner, b"members");
        let mm = make_store(&[make_block(
            &page(5),
            &make_node_pack(b"A", 0, std::slice::from_ref(&e)),
        )]);
        let (conn, st) = run(&mm);
        assert_eq!(st.collections, 1);
        let row: (i64, i64, String, i64, i64, String, i64) = conn
            .query_row("SELECT * FROM collections", [], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap();
        assert_eq!(
            row,
            (
                0x1234,
                0xd2,
                "Recipient".into(),
                0x99,
                0xc9,
                "MessageHeader".into(),
                e.len() as i64
            )
        );
    }

    #[test]
    fn unknown_type_gets_unlabelled_descriptors() {
        // 0x56 has no schema; 0xfff0 has no name either.
        let a = make_item(
            0x56,
            0x40,
            &[(0x10, b"hello world"), (0x20, &utf16("Ünï"))],
            &[],
        );
        let b = make_item(0xfff0, 0x20, &[(0x18, b"x y")], &[]);
        let mm = make_store(&[make_block(&page(1), &[a, b].concat())]);
        let (conn, st) = run(&mm);
        assert_eq!((st.objects, st.fields), (2, 3));
        let mut q = conn
            .prepare(
                "SELECT o.type_name, f.at, f.kind, f.label, f.text FROM fields f
                 JOIN objects o ON o.id = f.object_id ORDER BY o.id, f.at",
            )
            .unwrap();
        type R = (Option<String>, i64, String, Option<String>, Option<String>);
        let got: Vec<R> = q
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let name = names::type_name(0x56).map(String::from);
        assert_eq!(
            got,
            vec![
                (
                    name.clone(),
                    0x10,
                    "utf8".into(),
                    None,
                    Some("hello world".into())
                ),
                (name, 0x20, "utf16".into(), None, Some("Ünï".into())),
                (None, 0x18, "utf8".into(), None, Some("x y".into())),
            ]
        );
    }

    #[test]
    fn high_keys_and_hashes_round_trip_through_i64() {
        let key = 0xfedc_ba98_7654_3210u64;
        assert!(key >= 1 << 63);
        let owner = ObjRef {
            prefix: 0,
            page: u64::MAX,
            type_id: 0x55,
        };
        let r = recipient("Ann Lee", "a@b.c");
        let e = make_collection_entry(0x55, key, owner, b"");
        let mm = make_store(&[
            make_block(&page(key), &r),
            make_block(&page(3), &make_node_pack(&[], 0, &[e])),
        ]);
        let (conn, _) = run(&mm);
        let (p, h): (i64, i64) = conn
            .query_row("SELECT page, content_hash FROM objects", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(p as u64, key);
        assert_eq!(h as u64, fnv1a(&r[4..]));
        let (cp, op): (i64, i64) = conn
            .query_row("SELECT page, owner_page FROM collections", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((cp as u64, op as u64), (key, u64::MAX));
    }

    #[test]
    fn empty_store_exports_zero_rows() {
        let mm = make_store(&[]);
        let (conn, st) = run(&mm);
        assert_eq!(st, ExportStats::default());
        for t in ["objects", "fields", "collections"] {
            assert_eq!(count(&conn, &format!("SELECT COUNT(*) FROM {t}")), 0);
        }
        // The five type tables the views need exist even when empty.
        for t in ALWAYS_TABLES {
            let sql = format!("SELECT COUNT(*) FROM {}", props::quote_ident(t));
            assert_eq!(count(&conn, &sql), 0, "{t}");
        }
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'"
            ),
            3 + ALWAYS_TABLES.len() as i64
        );
    }

    #[test]
    fn synthetic_store_fills_the_three_tables_and_creates_all_seven_views() {
        let owner = ObjRef {
            prefix: 0,
            page: 0x99,
            type_id: 0xc9,
        };
        let e = make_collection_entry(0xd2, 0x1234, owner, b"members");
        let mm = make_store(&[
            make_block(&page(1), &recipient("Ann Lee", "ann@example.org")),
            make_block(&page(2), &make_node_pack(b"A", 0, &[e])),
        ]);
        let (conn, st) = run(&mm);
        assert_eq!((st.objects, st.fields, st.collections), (1, 2, 1));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM objects"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM fields"), 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM collections"), 1);
        assert_eq!(view_names(&conn), all_view_names());
    }

    /// Names of the views in the database, sorted.
    fn view_names(conn: &Connection) -> Vec<String> {
        let mut q = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'view' ORDER BY name")
            .unwrap();
        q.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The seven view names, sorted.
    fn all_view_names() -> Vec<String> {
        let mut want: Vec<String> = VIEWS.iter().map(|v| v.name.to_string()).collect();
        want.extend(COMPAT_VIEWS.iter().map(|(v, _)| v.to_string()));
        want.sort();
        want
    }

    #[test]
    fn there_are_seven_views() {
        assert_eq!(
            all_view_names(),
            [
                "folders",
                "message_data",
                "message_headers",
                "messages",
                "people",
                "recipients",
                "views"
            ]
        );
    }

    #[test]
    fn every_view_property_exists_in_the_schema() {
        let base = ["object_id", "guid", "page"];
        let has = |table: &str, column: &str| {
            let t = schema_named(table).unwrap_or_else(|| panic!("no schema {table}"));
            base.contains(&column) || t.props.iter().any(|p| props::column_name(p) == column)
        };
        for v in VIEWS {
            let table_of = |alias: &str| {
                v.sources
                    .iter()
                    .find(|s| s.alias == alias)
                    .unwrap_or_else(|| panic!("{}: no source {alias}", v.name))
                    .table
            };
            for (i, s) in v.sources.iter().enumerate() {
                assert!(ALWAYS_TABLES.contains(&s.table), "{}: {}", v.name, s.table);
                // The first source has no join; every later one joins an
                // earlier source's reference property.
                assert_eq!(s.on.is_none(), i == 0, "{}", v.name);
                if let Some((left, prop)) = s.on {
                    let before = v.sources[..i].iter().any(|e| e.alias == left);
                    assert!(before, "{}: {left} is not an earlier source", v.name);
                    let t = schema_named(table_of(left)).unwrap();
                    let p = t.props.iter().find(|p| p.name == prop);
                    assert_eq!(p.map(|p| p.size), Some(48), "{}: {prop} is no ref", v.name);
                }
            }
            for c in v.columns {
                let table = table_of(c.alias);
                assert!(has(table, c.column), "{}: {table}.{}", v.name, c.column);
            }
        }
        for (view, table) in COMPAT_VIEWS {
            assert!(ALWAYS_TABLES.contains(&table), "{view}: {table}");
        }
    }

    #[test]
    fn view_sql_quotes_every_identifier() {
        let sql = view_sql(&VIEWS[1]);
        assert_eq!(
            sql,
            "CREATE VIEW \"folders\" AS SELECT \"v\".\"guid\" AS \"guid\", \
             \"v\".\"Name\" AS \"Name\", \"v\".\"Type\" AS \"Type\", \
             \"v\".\"TotalCount\" AS \"TotalCount\", \"v\".\"UnreadCount\" AS \"UnreadCount\", \
             \"p\".\"Name\" AS \"parent\", \"p\".\"guid\" AS \"parent_guid\" \
             FROM \"View\" \"v\" LEFT JOIN \"View\" \"p\" ON \"v\".\"ParentView\" = \"p\".\"guid\""
        );
    }

    const GD: [u8; 16] = [0xca; 16];
    const GP: [u8; 16] = [0x50; 16];

    #[test]
    fn messages_view_joins_folder_and_body() {
        let linked = make_typed_item(
            "MessageHeader",
            G1,
            &[
                ("Subject", PropValue::Text("שלום".into())),
                (
                    "SenderEmailAddress",
                    PropValue::Text("ann@example.org".into()),
                ),
                ("IsRead", PropValue::Bool(true)),
                (
                    "View",
                    PropValue::Ref {
                        type_id: 0x4d,
                        guid: GV,
                    },
                ),
                (
                    "MessageData",
                    PropValue::Ref {
                        type_id: 0xca,
                        guid: GD,
                    },
                ),
            ],
        );
        // Its View GUID names no object in the store.
        let orphan = make_typed_item(
            "MessageHeader",
            G2,
            &[
                ("Subject", PropValue::Text("orphan".into())),
                (
                    "View",
                    PropValue::Ref {
                        type_id: 0x4d,
                        guid: [0x77; 16],
                    },
                ),
            ],
        );
        let view = make_typed_item("View", GV, &[("Name", PropValue::Text("Inbox".into()))]);
        let data = make_typed_item(
            "MessageData",
            GD,
            &[
                ("HTMLBodyBytes", PropValue::Bytes(b"<p>hi</p>".to_vec())),
                ("BodyEncoding", PropValue::Int(3)),
            ],
        );
        let mm = make_store(&[make_block(&page(1), &[linked, orphan, view, data].concat())]);
        let (conn, _) = run(&mm);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 2);
        type R = (String, String, i64, String, String, Vec<u8>, i64);
        let got: R = conn
            .query_row(
                "SELECT Subject, SenderEmailAddress, IsRead, folder, folder_guid,
                        CAST(body_html AS BLOB), body_encoding
                 FROM messages WHERE guid = ?1",
                [hex(G1)],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            got,
            (
                "שלום".into(),
                "ann@example.org".into(),
                1,
                "Inbox".into(),
                hex(GV),
                b"<p>hi</p>".to_vec(),
                3
            )
        );
        let (subject, folder, read): (String, Option<String>, i64) = conn
            .query_row(
                "SELECT Subject, folder, IsRead FROM messages WHERE guid = ?1",
                [hex(G2)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((subject.as_str(), folder, read), ("orphan", None, 0));
    }

    #[test]
    fn folders_view_names_the_parent() {
        let parent = make_typed_item("View", GP, &[("Name", PropValue::Text("Top".into()))]);
        let child = make_typed_item(
            "View",
            GV,
            &[
                ("Name", PropValue::Text("Child".into())),
                ("TotalCount", PropValue::Int(5)),
                ("UnreadCount", PropValue::Int(2)),
                (
                    "ParentView",
                    PropValue::Ref {
                        type_id: 0x4d,
                        guid: GP,
                    },
                ),
            ],
        );
        let mm = make_store(&[make_block(&page(1), &[parent, child].concat())]);
        let (conn, _) = run(&mm);
        type R = (String, i64, i64, Option<String>, Option<String>);
        let row = |g: [u8; 16]| -> R {
            conn.query_row(
                "SELECT Name, TotalCount, UnreadCount, parent, parent_guid
                 FROM folders WHERE guid = ?1",
                [hex(g)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
        };
        assert_eq!(
            row(GV),
            ("Child".into(), 5, 2, Some("Top".into()), Some(hex(GP)))
        );
        assert_eq!(row(GP), ("Top".into(), 0, 0, None, None));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM views"), 2);
    }

    #[test]
    fn recipients_and_people_views_return_identity_and_address() {
        let r = make_typed_item(
            "Recipient",
            G1,
            &[
                ("DisplayName", PropValue::Text("Ann Lee".into())),
                ("EmailAddress", PropValue::Text("ann@example.org".into())),
                ("GivenName", PropValue::Text("Ann".into())),
            ],
        );
        let p = make_typed_item(
            "Person",
            G2,
            &[
                ("DisplayName", PropValue::Text("Bob Roe".into())),
                ("EmailAddress", PropValue::Text("bob@y.z".into())),
            ],
        );
        let mm = make_store(&[make_block(&page(7), &r), make_block(&page(8), &p)]);
        let (conn, _) = run(&mm);
        type R = (i64, i64, String, String, String);
        let get = |sql: &str| -> R {
            conn.query_row(sql, [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
        };
        let id = |guid: [u8; 16]| -> i64 {
            conn.query_row("SELECT id FROM objects WHERE guid = ?1", [hex(guid)], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(
            get("SELECT object_id, page, guid, display_name, email FROM recipients"),
            (
                id(G1),
                7,
                hex(G1),
                "Ann Lee".into(),
                "ann@example.org".into()
            )
        );
        let given: (String, Option<String>) = conn
            .query_row("SELECT GivenName, Surname FROM recipients", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(given, ("Ann".into(), None));
        assert_eq!(
            get("SELECT object_id, page, guid, name, email FROM people"),
            (id(G2), 8, hex(G2), "Bob Roe".into(), "bob@y.z".into())
        );
    }

    #[test]
    fn store_without_mail_objects_has_all_views_empty() {
        // Only an object of a type without schema.
        let other = make_item(0x56, 0x40, &[(0x10, b"no schema")], &[]);
        let mm = make_store(&[make_block(&page(1), &other)]);
        let (conn, st) = run(&mm);
        assert_eq!((st.generic_objects, st.schema_objects), (1, 0));
        assert_eq!(view_names(&conn), all_view_names());
        for v in all_view_names() {
            let sql = format!("SELECT COUNT(*) FROM {}", props::quote_ident(&v));
            assert_eq!(count(&conn, &sql), 0, "{v}");
        }
    }

    const G1: [u8; 16] = [0x11; 16];
    const G2: [u8; 16] = [0x22; 16];
    const GV: [u8; 16] = [0x4d; 16];
    const SENT: i64 = 1_709_251_200;

    fn header(guid: [u8; 16], subject: &str, read: bool) -> Vec<u8> {
        make_typed_item(
            "MessageHeader",
            guid,
            &[
                ("Subject", PropValue::Text(subject.into())),
                ("SentTime", PropValue::Time(SENT)),
                ("IsRead", PropValue::Bool(read)),
                ("Importance", PropValue::Int(2)),
                (
                    "View",
                    PropValue::Ref {
                        type_id: 0x4d,
                        guid: GV,
                    },
                ),
            ],
        )
    }

    fn typed_recipient(guid: [u8; 16], name: &str) -> Vec<u8> {
        make_typed_item(
            "Recipient",
            guid,
            &[("DisplayName", PropValue::Text(name.into()))],
        )
    }

    fn hex(g: [u8; 16]) -> String {
        props::guid_hex(&g)
    }

    #[test]
    fn schema_objects_get_one_row_per_guid_from_the_newest_copy() {
        let view = make_typed_item("View", GV, &[("Name", PropValue::Text("Inbox".into()))]);
        // The later block holds the lower page key, so key order (page 1
        // first) and file order (page 2 first) disagree.
        let mm = make_store(&[
            make_block(&page(2), &header(G1, "old", false)),
            make_block(&page(1), &header(G1, "new", true)),
            make_block(&page(3), &[header(G2, "second", false), view].concat()),
        ]);
        let (conn, st) = run(&mm);
        assert_eq!(
            (st.schema_objects, st.generic_objects, st.objects),
            (3, 0, 3)
        );
        assert_eq!((st.superseded, st.duplicates), (1, 0));
        assert_eq!((st.fixed_len_mismatch, st.no_guid), (0, 0));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM \"MessageHeader\""), 2);

        type R = (String, i64, i64, i64, String, i64);
        let got: R = conn
            .query_row(
                "SELECT Subject, IsRead, SentTime, Importance, View, page
                 FROM \"MessageHeader\" WHERE guid = ?1",
                [hex(G1)],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(got, ("new".into(), 1, SENT, 2, hex(GV), 1));
        let read: i64 = conn
            .query_row(
                "SELECT IsRead FROM \"MessageHeader\" WHERE guid = ?1",
                [hex(G2)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(read, 0);

        // objects carries the same identity and location as the type table.
        let (page, has_schema): (i64, i64) = conn
            .query_row(
                "SELECT page, has_schema FROM objects WHERE guid = ?1",
                [hex(G1)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((page, has_schema), (1, 1));
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM \"MessageHeader\" t JOIN objects o ON o.id = t.object_id
                 WHERE o.guid = t.guid AND o.page = t.page AND o.has_schema = 1
                   AND o.type_id = 201"
            ),
            2
        );
        // A table named by an SQL keyword, and the reference joins to it.
        let name: String = conn
            .query_row(
                "SELECT v.Name FROM \"MessageHeader\" h JOIN \"View\" v ON h.View = v.guid
                 WHERE h.guid = ?1",
                [hex(G1)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "Inbox");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM fields"), 0);
    }

    /// `(Subject, page)` of the only `MessageHeader` row with GUID `G1`.
    fn only_header(conn: &Connection) -> (String, i64) {
        assert_eq!(count(conn, "SELECT COUNT(*) FROM \"MessageHeader\""), 1);
        conn.query_row(
            "SELECT Subject, page FROM \"MessageHeader\" WHERE guid = ?1",
            [hex(G1)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn newer_copy_in_a_later_page_replaces_the_first_one_seen() {
        // Key order and file order agree: the index sees the old copy first.
        let mm = make_store(&[
            make_block(&page(1), &header(G1, "old", false)),
            make_block(&page(2), &header(G1, "new", true)),
        ]);
        let (conn, st) = run(&mm);
        assert_eq!((st.schema_objects, st.superseded, st.objects), (1, 1, 1));
        assert_eq!(only_header(&conn), ("new".into(), 2));
    }

    #[test]
    fn same_guid_twice_in_one_page_exports_the_copy_at_the_higher_offset() {
        let old = header(G1, "old", false);
        let second = (old.len() + 4) as i64;
        let mm = make_store(&[make_block(
            &page(7),
            &[old, header(G1, "new", true)].concat(),
        )]);
        let (conn, st) = run(&mm);
        assert_eq!((st.schema_objects, st.superseded, st.objects), (1, 1, 1));
        assert_eq!(only_header(&conn), ("new".into(), 7));
        assert_eq!(
            count(
                &conn,
                "SELECT item_offset FROM objects WHERE guid IS NOT NULL"
            ),
            second
        );
    }

    #[test]
    fn passes_that_disagree_are_an_error() {
        assert!(check_passes((2, 1), (2, 1)).is_ok());
        for written in [(1, 1), (2, 0)] {
            let e = check_passes((2, 1), written).unwrap_err();
            assert!(matches!(
                e,
                ExportError::StoreChanged { indexed: (2, 1), written: w } if w == written
            ));
            let msg = e.to_string();
            assert!(msg.contains("changed between the two passes"), "{msg}");
            assert!(msg.contains("snapshot copy"), "{msg}");
        }
    }

    #[test]
    fn items_without_identity_take_the_generic_path_and_are_counted() {
        let mut wrong = typed_recipient(G1, "Wrong Size");
        let fl = u16::from_le_bytes([wrong[6], wrong[7]]) - 2;
        set_fixed_len(&mut wrong, fl);
        let zero = typed_recipient([0; 16], "Zero Guid");
        let unknown = make_item(0x56, 0x40, &[(0x10, b"no schema")], &[]);
        let good = typed_recipient(G2, "Good");
        let mm = make_store(&[
            make_block(&page(1), &wrong),
            make_block(&page(2), &zero),
            make_block(&page(3), &[unknown, good].concat()),
        ]);
        let (conn, st) = run(&mm);
        assert_eq!((st.fixed_len_mismatch, st.no_guid), (1, 1));
        assert_eq!((st.schema_objects, st.generic_objects), (1, 3));
        assert_eq!(st.objects, st.schema_objects + st.generic_objects);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM \"Recipient\""), 1);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM objects WHERE guid IS NULL AND has_schema = 0"
            ),
            3
        );
        // The zero-GUID Recipient and the type without schema have fields rows.
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM fields WHERE text IN ('Zero Guid', 'no schema')"
            ),
            2
        );
        let name: String = conn
            .query_row("SELECT DisplayName FROM \"Recipient\"", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Good");
    }

    #[test]
    fn schema_and_generic_rows_may_share_a_content_hash() {
        let r = typed_recipient(G1, "Ann Lee");
        // An exact copy in another page: superseded, not a generic duplicate.
        let mm = make_store(&[make_block(&page(1), &r), make_block(&page(2), &r)]);
        let (conn, st) = run(&mm);
        assert_eq!((st.schema_objects, st.superseded, st.duplicates), (1, 1, 0));
        let (h, size, type_id): (i64, i64, i64) = conn
            .query_row("SELECT content_hash, size, type_id FROM objects", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(h as u64, fnv1a(&r[4..]));
        // A generic row with the same (content_hash, size, type_id) inserts.
        let ins = "INSERT OR IGNORE INTO objects (type_id, size, content_hash) VALUES (?1, ?2, ?3)";
        assert_eq!(conn.execute(ins, params![type_id, size, h]).unwrap(), 1);
        // A second schema row of the same (type_id, guid) is an error.
        let dup = conn.execute(
            "INSERT INTO objects (type_id, guid) VALUES (?1, ?2)",
            params![type_id, hex(G1)],
        );
        assert!(dup.is_err());
    }

    #[test]
    fn refused_schema_field_is_null_and_counted() {
        // Six string properties all name the Subject's heap bytes; the
        // budget (4x the heap) takes four.
        let mut v = make_typed_item(
            "MessageHeader",
            G1,
            &[("Subject", PropValue::Text("same bytes".into()))],
        );
        let t = props::schema_for(0xc9).unwrap();
        let at = |name: &str| t.props.iter().find(|p| p.name == name).unwrap().offset as usize;
        let others = [
            "Preview",
            "NormalizedSubject",
            "SenderDisplayName",
            "SenderEmailAddress",
            "InternetMessageId",
        ];
        for name in others {
            copy_descriptor(&mut v, at("Subject"), at(name));
        }
        let mm = make_store(&[make_block(&page(1), &v)]);
        let (conn, st) = run(&mm);
        assert_eq!((st.schema_objects, st.skipped_fields), (1, 2));
        let set = count(
            &conn,
            "SELECT (Subject IS NOT NULL) + (Preview IS NOT NULL)
                  + (NormalizedSubject IS NOT NULL) + (SenderDisplayName IS NOT NULL)
                  + (SenderEmailAddress IS NOT NULL) + (InternetMessageId IS NOT NULL)
             FROM \"MessageHeader\"",
        );
        assert_eq!(set, 4);
    }

    #[test]
    fn type_tables_are_created_on_first_object_only() {
        let person = make_typed_item("Person", G1, &[]);
        let root = make_typed_item("Root", G2, &[]);
        let mm = make_store(&[make_block(&page(1), &[person, root].concat())]);
        let (conn, st) = run(&mm);
        assert_eq!(st.schema_objects, 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM \"Root\""), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM \"Person\""), 1);
        // A type with a schema but no object has no table.
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'Error'"
            ),
            0
        );
    }

    #[test]
    fn type_table_sql_quotes_and_orders_columns() {
        let t = props::schema_for(0x4d).unwrap();
        let sql = type_table_sql(t);
        assert!(sql.starts_with("CREATE TABLE \"View\"(object_id INTEGER PRIMARY KEY"));
        assert!(sql.contains("\"Name\" TEXT"), "{sql}");
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&sql).unwrap();
        let q = conn.prepare("SELECT * FROM \"View\"").unwrap();
        let cols = q.column_names();
        assert_eq!(cols.len(), 3 + t.props.len());
        assert_eq!(&cols[..3], ["object_id", "guid", "page"]);
        let want: Vec<String> = t.props.iter().map(props::column_name).collect();
        assert_eq!(
            cols[3..].iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            want
        );
    }
}
