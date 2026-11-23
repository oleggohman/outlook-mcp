//! Opt-in integration test against a real `HxStore.hxd` snapshot.
//!
//! Runs only when `HXSTORE_SNAPSHOT` names a store file; otherwise it prints a
//! skip line and passes. It asserts and prints aggregate facts only (counts and
//! ratios), never field contents, so its output is safe to share.
//!
//! Thresholds are deliberately looser than the values measured on the
//! reference store, so the test also passes on other stores.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use hxprobe::item::{self, Text};
use hxprobe::kv::Store;
use hxprobe::names;
use hxprobe::props::{self, TypeSchema};
use hxprobe::value::{self, Value};
use memmap2::Mmap;

const RECIPIENT: u16 = 0xd2;
const PERSON: u16 = 0x55;
const MESSAGE_HEADER: u16 = 0xc9;
const MESSAGE_DATA: u16 = 0xca;

/// Removes the export database (and SQLite side files) when dropped, so the
/// file goes away even when an assertion fails.
struct TempDb(PathBuf);

impl Drop for TempDb {
    fn drop(&mut self) {
        let base = self.0.as_os_str().to_owned();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = base.clone();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

/// Matches / total, as a ratio; 0 when nothing was counted.
#[derive(Default)]
struct Ratio {
    hits: usize,
    total: usize,
}

impl Ratio {
    fn add(&mut self, hit: bool) {
        self.total += 1;
        self.hits += hit as usize;
    }

    fn value(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.hits as f64 / self.total as f64
        }
    }
}

/// A simple email shape: exactly one `@`, non-empty local and domain parts,
/// no whitespace.
fn looks_like_email(s: &str) -> bool {
    let mut parts = s.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => {
            !local.is_empty() && !domain.is_empty() && !s.chars().any(char::is_whitespace)
        }
        _ => false,
    }
}

fn text_of(b: &[u8]) -> Option<String> {
    match item::decode_text(b) {
        Text::Utf16(s) | Text::Utf8(s) => Some(s),
        Text::Binary => None,
    }
}

/// Per-type tally of schema-typed items, counted independently of the export.
#[derive(Default)]
struct SchemaTally {
    /// Items of the type.
    items: usize,
    /// Items whose `fixed_len` differs from the schema.
    mismatch: usize,
    /// Items with the schema's `fixed_len` but an all-zero GUID.
    no_guid: usize,
    /// Items with the schema's `fixed_len` and a non-zero GUID.
    copies: usize,
    /// Distinct non-zero GUIDs of those items.
    guids: HashSet<[u8; 16]>,
}

impl SchemaTally {
    /// Copies beyond the first of each GUID.
    fn superseded(&self) -> usize {
        self.copies - self.guids.len()
    }
}

/// The 16 bytes at item offset 12, read directly (not through `props`);
/// `None` when all zero or the item is too short.
fn own_guid(b: &[u8]) -> Option<[u8; 16]> {
    let g: [u8; 16] = b.get(12..28)?.try_into().ok()?;
    (g != [0; 16]).then_some(g)
}

/// `n / d` in percent; 0 when `d` is 0.
fn pct(n: i64, d: i64) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 * 100.0 / d as f64
    }
}

fn schema(type_id: u16) -> &'static TypeSchema {
    props::schema_for(type_id).unwrap_or_else(|| panic!("type {type_id:#x} has no schema"))
}

/// Item offset of the variable (string) property `name` of `t`.
fn var_prop_at(t: &TypeSchema, name: &str) -> usize {
    let p = t
        .props
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("{} has no property {name}", t.name));
    assert_eq!(p.size, 0, "{}.{name} is not a variable field", t.name);
    p.offset as usize
}

#[test]
fn real_snapshot_aggregate_facts() {
    let Some(path) = std::env::var_os("HXSTORE_SNAPSHOT") else {
        println!("skipped: HXSTORE_SNAPSHOT not set");
        return;
    };
    let started = Instant::now();
    let f = File::open(&path).expect("open HXSTORE_SNAPSHOT");
    // SAFETY: opened read-only and only read through the slice.
    let mm = unsafe { Mmap::map(&f) }.expect("mmap HXSTORE_SNAPSHOT");

    // Container facts.
    let store = Store::load(&mm);
    let s = &store.stats;
    let failed_ratio = s.failed as f64 / s.blocks.max(1) as f64;
    println!(
        "blocks: signatures {} failed {} ({:.4}%) over_budget {} page_keys {} system_keys {} stale {}",
        s.blocks,
        s.failed,
        failed_ratio * 100.0,
        s.over_budget,
        s.page_keys,
        s.system_keys,
        s.stale_revisions
    );
    assert!(s.blocks > 0, "no block signatures found");
    assert!(failed_ratio < 0.01, "failed signatures >= 1%");
    assert!(s.page_keys > 0, "no 8-byte keys");
    assert!(s.system_keys > 0, "no 16-byte keys");

    // One pass over live page values: classification and item facts.
    let (rs, ps) = (schema(RECIPIENT), schema(PERSON));
    let (r_name, r_email) = (
        var_prop_at(rs, "DisplayName"),
        var_prop_at(rs, "EmailAddress"),
    );
    let p_email = var_prop_at(ps, "EmailAddress");

    let (mut node_packs, mut coll_pages, mut other) = (0usize, 0usize, 0usize);
    let mut fixed_lens: BTreeMap<u16, BTreeSet<usize>> = BTreeMap::new();
    let mut counts: BTreeMap<u16, usize> = BTreeMap::new();
    let mut items = 0usize;
    let mut r_email_ok = Ratio::default();
    let mut r_name_text = Ratio::default();
    let mut p_email_ok = Ratio::default();
    let mut tallies: BTreeMap<u16, SchemaTally> = BTreeMap::new();

    for &key in store.pages.keys() {
        let Some(data) = store.page_value(&mm, key) else {
            other += 1;
            continue;
        };
        match value::classify(key, &data) {
            Value::NodePack { .. } => node_packs += 1,
            Value::CollectionPage { .. } => coll_pages += 1,
            Value::Other => other += 1,
        }
        for it in item::scan(&data) {
            items += 1;
            *counts.entry(it.type_id).or_default() += 1;
            fixed_lens
                .entry(it.type_id)
                .or_default()
                .insert(it.fixed_len);
            if let Some(t) = props::schema_for(it.type_id) {
                let tally = tallies.entry(it.type_id).or_default();
                tally.items += 1;
                if it.fixed_len != t.fixed_len as usize {
                    tally.mismatch += 1;
                } else if let Some(g) = own_guid(it.bytes) {
                    tally.copies += 1;
                    tally.guids.insert(g);
                } else {
                    tally.no_guid += 1;
                }
            }
            // Content checks only on items the schema applies to.
            match it.type_id {
                RECIPIENT if props::applies(rs, &it) => {
                    if let Some(b) = it.var_at(r_email) {
                        r_email_ok.add(text_of(b).is_some_and(|t| looks_like_email(&t)));
                    }
                    if let Some(b) = it.var_at(r_name) {
                        r_name_text.add(text_of(b).is_some());
                    }
                }
                PERSON if props::applies(ps, &it) => {
                    if let Some(b) = it.var_at(p_email) {
                        p_email_ok.add(text_of(b).is_some_and(|t| looks_like_email(&t)));
                    }
                }
                _ => {}
            }
        }
    }

    let classified = node_packs + coll_pages;
    let live = classified + other;
    let classified_ratio = classified as f64 / live.max(1) as f64;
    println!(
        "values: live {live} node_packs {node_packs} collection_pages {coll_pages} other {other} ({:.4}% classified)",
        classified_ratio * 100.0
    );
    assert!(classified_ratio >= 0.95, "< 95% of live values classified");

    println!("items: {items} in {} types (before dedup)", counts.len());
    for id in [MESSAGE_HEADER, MESSAGE_DATA, RECIPIENT, PERSON] {
        let fixed_len = schema(id).fixed_len as usize;
        let name = names::type_name(id).unwrap_or("?");
        let n = counts.get(&id).copied().unwrap_or(0);
        let lens = fixed_lens.get(&id).cloned().unwrap_or_default();
        println!("type {id:#x} {name}: {n} items, fixed_lens {lens:x?} (schema {fixed_len:#x})");
        assert!(n > 0, "{name} not found");
        assert!(
            lens.contains(&fixed_len),
            "{name} never has the schema fixed_len {fixed_len:#x}"
        );
    }

    println!(
        "Recipient email: {} of {} look like email ({:.4}%)",
        r_email_ok.hits,
        r_email_ok.total,
        r_email_ok.value() * 100.0
    );
    println!(
        "Recipient display_name: {} of {} decode as text ({:.4}%)",
        r_name_text.hits,
        r_name_text.total,
        r_name_text.value() * 100.0
    );
    println!(
        "Person email: {} of {} look like email ({:.4}%)",
        p_email_ok.hits,
        p_email_ok.total,
        p_email_ok.value() * 100.0
    );
    assert!(r_email_ok.value() >= 0.95, "Recipient email < 95%");
    assert!(
        r_name_text.value() >= 0.95,
        "Recipient display_name text < 95%"
    );
    assert!(p_email_ok.value() >= 0.95, "Person email < 95%");

    // Schema agreement: share of schema-typed items with the schema's size.
    let schema_items: usize = tallies.values().map(|t| t.items).sum();
    let schema_mismatch: usize = tallies.values().map(|t| t.mismatch).sum();
    let schema_match = schema_items - schema_mismatch;
    let agreement = pct(schema_match as i64, schema_items as i64);
    println!(
        "schema agreement: {schema_match} of {schema_items} schema-typed items have the schema fixed_len ({agreement:.4}%) in {} types",
        tallies.len()
    );
    assert!(agreement >= 95.0, "schema fixed_len agreement < 95%");
    for id in [RECIPIENT, PERSON] {
        let t = tallies.get(&id).expect("tally");
        println!(
            "{}: no_guid {} fixed_len_mismatch {} superseded {} distinct {}",
            schema(id).name,
            t.no_guid,
            t.mismatch,
            t.superseded(),
            t.guids.len()
        );
    }
    let header_guids = tallies.get(&MESSAGE_HEADER).map_or(0, |t| t.guids.len());

    // Export facts: a real file under the temp dir, removed by the guard.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let db = TempDb(std::env::temp_dir().join(format!(
        "hxprobe-snapshot-{}-{nanos}.db",
        std::process::id()
    )));
    {
        let conn = rusqlite::Connection::open(&db.0).expect("open temp db");
        let export_started = Instant::now();
        let st = hxprobe::export::export(&store, &mm, &conn, item::tick_window()).expect("export");
        let export_secs = export_started.elapsed().as_secs_f64();
        let objects: i64 = conn
            .query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0))
            .expect("count objects");
        let count = |sql: &str| -> i64 {
            conn.query_row(sql, [], |r| r.get(0))
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
        };
        let recipients = count("SELECT COUNT(*) FROM recipients");
        let recipient_rows = count("SELECT COUNT(*) FROM \"Recipient\"");
        let person_rows = count("SELECT COUNT(*) FROM \"Person\"");
        let recipients_email =
            count("SELECT COUNT(*) FROM recipients WHERE email IS NOT NULL AND email != ''");
        println!(
            "export: values {} objects {} duplicates {} fields {} collections {} skipped_items {} skipped_fields {}; objects rows {objects}, recipients rows {recipients} (with email {recipients_email}), Recipient rows {recipient_rows}, Person rows {person_rows}",
            st.values,
            st.objects,
            st.duplicates,
            st.fields,
            st.collections,
            st.skipped_items,
            st.skipped_fields
        );
        assert!(objects > 0, "export wrote no objects");
        assert!(recipients > 0, "recipients view is empty");
        assert_eq!(
            recipients, recipient_rows,
            "recipients view != Recipient table"
        );
        assert!(person_rows > 0, "Person table is empty");
        assert!(
            recipients_email as f64 >= 0.95 * recipients as f64,
            "recipients with email < 95%"
        );
        println!(
            "export stats: schema_objects {} generic_objects {} no_guid {} fixed_len_mismatch {} superseded {}; export {export_secs:.1}s",
            st.schema_objects, st.generic_objects, st.no_guid, st.fixed_len_mismatch, st.superseded
        );

        // Rows per type table, top 15 (table names are schema type names).
        let mut tables: Vec<(String, i64)> = {
            let mut q = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' \
                     AND name NOT IN ('objects', 'fields', 'collections')",
                )
                .expect("list tables");
            let names: Vec<String> = q
                .query_map([], |r| r.get(0))
                .expect("query tables")
                .map(|r| r.expect("table name"))
                .collect();
            names
                .into_iter()
                .map(|n| {
                    let c = count(&format!("SELECT COUNT(*) FROM {}", props::quote_ident(&n)));
                    (n, c)
                })
                .collect()
        };
        tables.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        println!("type tables: {}", tables.len());
        for (n, c) in tables.iter().take(15) {
            println!("  {n}: {c} rows");
        }

        // Identity: one row per distinct GUID.
        let headers = count("SELECT COUNT(*) FROM \"MessageHeader\"");
        println!("MessageHeader rows {headers}, distinct live GUIDs {header_guids}");
        assert_eq!(
            headers, header_guids as i64,
            "MessageHeader rows != distinct GUIDs"
        );

        // Links.
        let view_refs = count("SELECT COUNT(*) FROM \"MessageHeader\" WHERE \"View\" IS NOT NULL");
        let view_ok = count(
            "SELECT COUNT(*) FROM \"MessageHeader\" h WHERE h.\"View\" IS NOT NULL \
             AND EXISTS (SELECT 1 FROM \"View\" v WHERE v.guid = h.\"View\")",
        );
        let data_refs =
            count("SELECT COUNT(*) FROM \"MessageHeader\" WHERE \"MessageData\" IS NOT NULL");
        let data_ok = count(
            "SELECT COUNT(*) FROM \"MessageHeader\" h WHERE h.\"MessageData\" IS NOT NULL \
             AND EXISTS (SELECT 1 FROM \"MessageData\" d WHERE d.guid = h.\"MessageData\")",
        );
        let (view_pct, data_pct) = (pct(view_ok, view_refs), pct(data_ok, data_refs));
        println!("links: View {view_ok} of {view_refs} resolve ({view_pct:.4}%)");
        println!("links: MessageData {data_ok} of {data_refs} resolve ({data_pct:.4}%)");
        assert!(view_refs > 0 && view_pct >= 99.0, "View links < 99%");
        assert!(data_refs > 0 && data_pct >= 95.0, "MessageData links < 95%");

        // IsRead check: folders whose exported message count equals
        // TotalCount and whose IsRead = 0 count equals UnreadCount.
        let (big, matching, largest): (i64, i64, Option<i64>) = conn
            .query_row(
                "WITH f AS (SELECT COUNT(*) AS n, SUM(h.IsRead = 0) AS unread, \
                        v.TotalCount AS total, v.UnreadCount AS unread_count \
                   FROM \"MessageHeader\" h JOIN \"View\" v ON h.\"View\" = v.guid \
                   GROUP BY v.guid) \
                 SELECT (SELECT COUNT(*) FROM f WHERE n >= 100), COUNT(*), MAX(n) FROM f \
                 WHERE n >= 100 AND n = total AND unread = unread_count",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("IsRead check");
        println!(
            "IsRead check: {matching} of {big} folders with >= 100 messages match TotalCount and UnreadCount (largest {})",
            largest.unwrap_or(0)
        );
        assert!(matching >= 1, "no folder passes the IsRead check");

        // Content shares (counts only).
        let share = |label: &str, cond: &str, table: &str| -> f64 {
            let t = props::quote_ident(table);
            let total = count(&format!("SELECT COUNT(*) FROM {t}"));
            let n = count(&format!("SELECT COUNT(*) FROM {t} WHERE {cond}"));
            let p = pct(n, total);
            println!("{table} {label}: {n} of {total} ({p:.4}%)");
            p
        };
        share(
            "Subject non-empty",
            "Subject IS NOT NULL AND Subject != ''",
            "MessageHeader",
        );
        share(
            "SenderEmailAddress non-empty",
            "SenderEmailAddress IS NOT NULL AND SenderEmailAddress != ''",
            "MessageHeader",
        );
        share("SentTime non-NULL", "SentTime IS NOT NULL", "MessageHeader");
        // Two-heap guards: with a single-heap rule these were near 0%.
        let ipm = share(
            "ItemClass starts with IPM.",
            "substr(ItemClass, 1, 4) = 'IPM.'",
            "MessageHeader",
        );
        let one_at = share(
            "SenderEmailAddress with exactly one @",
            "length(SenderEmailAddress) - length(replace(SenderEmailAddress, '@', '')) = 1",
            "MessageHeader",
        );
        share(
            "HTMLBodyBytes over 1000 bytes",
            "LENGTH(CAST(HTMLBodyBytes AS BLOB)) > 1000",
            "MessageData",
        );
        assert!(ipm >= 95.0, "ItemClass IPM. < 95%");
        assert!(one_at >= 95.0, "SenderEmailAddress with one @ < 95%");
    }
    let db_size = std::fs::metadata(&db.0).map_or(0, |m| m.len());
    println!("database size: {:.1} MB", db_size as f64 / 1e6);
    let db_path = db.0.clone();
    drop(db);
    assert!(!db_path.exists(), "temp db was not removed");
    println!("elapsed: {:.1}s", started.elapsed().as_secs_f64());
}
