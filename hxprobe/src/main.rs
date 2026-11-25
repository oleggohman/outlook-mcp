//! `hxprobe` — a parser for `HxStore.hxd`, the New Outlook for Mac message store.
//!
//! The store is a paged block container: each block carries two CRC-32s and an
//! LZ4-compressed payload holding one or more message records. Because the
//! checksums make every block self-validating, this is a parser rather than a
//! carver — a wrong offset fails loudly instead of yielding plausible garbage.
//!
//! See `SPEC.md` for the format itself. Commands:
//!
//! ```text
//!   hxprobe blocks <file>            verify every block, report coverage
//!   hxprobe types  <file>            object type histogram: id, name, count, schema, fixed_len
//!   hxprobe dump   <file> <type> [n] fields of n objects of a type (id or name)
//!   hxprobe export <file> [out.db] [--force]  structural SQLite export
//!   hxprobe db     <file> [out.db]   build a SQLite database with FTS5 search
//!   hxprobe map    <file> [n]        dump the named field map for n records
//!   hxprobe find   <file> <term>     show records whose text contains <term>
//!   hxprobe mcp [--store <file>] [--cache-dir <dir>] [--refresh-secs <n>]
//!                                    MCP server over stdin/stdout
//!   hxprobe service [--store <file>] [--cache-dir <dir>] [--refresh-secs <n>]
//!                   [--idle-secs <n>]
//!                                    background builder of the MCP cache
//!   hxprobe service --status|--stop [--cache-dir <dir>]
//!                                    show or stop that builder
//!   hxprobe --version                print the version
//! ```
//!
//! The file is memory-mapped throughout and never read into an owned buffer, so
//! peak RSS stays near the working set (~67 MB on a 56 MB store) rather than
//! tracking file size.

use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use hxprobe::{block, export, header, inspect, item, kv, names, record, schema};

/// Memory-map a store file read-only.
///
/// Mapping rather than reading matters: these stores reach a gigabyte, and
/// `read()`-ing one into a `Vec` is an easy way to exhaust a machine.
fn map(path: &str) -> Mmap {
    let f = File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    // SAFETY: the file is opened read-only and only read through the slice.
    // A concurrent writer could tear reads, which is why the documented
    // workflow is to snapshot the store while Outlook is closed.
    let mm = unsafe { Mmap::map(&f) }.unwrap_or_else(|e| panic!("mmap {path}: {e}"));

    // Check the header before doing anything else, so pointing this at the
    // wrong file says so instead of reporting zero blocks.
    match header::check(&mm) {
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
        Ok(h) if !h.known => {
            eprintln!(
                "{path}: warning: store version {:?} (0x{:02x}) is untested; \
                 known versions are 'i' (macOS) and 'h' (Windows Mail).\n\
                 Parsing anyway: every block is checksummed, so bad data fails \
                 loudly rather than silently.",
                h.version as char, h.version
            );
        }
        Ok(_) => {}
    }
    mm
}

/// One message record, located and bounded inside a decompressed block.
struct Located<'a> {
    /// Bytes of the record: from the previous anchor to the next.
    span: &'a [u8],
    /// Offset of the anchor within the block payload.
    anchor: usize,
    /// Distance from the anchor back to the start of the span.
    back: usize,
    /// Offset of the anchor's record end within the block payload.
    end: usize,
}

/// Split a decompressed block into its records.
///
/// Records sit back to back with no length field, so each one is bounded by the
/// neighbouring `IPM.Note` anchors. Without that bound fields bleed between
/// messages and pair one message's sender with another's subject.
fn records<'a>(data: &'a [u8], needle: &[u8]) -> Vec<Located<'a>> {
    memchr::memmem::find_iter(data, needle)
        .map(|anchor| {
            let end = memchr::memmem::find(&data[anchor + 2..], needle)
                .map_or(data.len(), |p| anchor + 2 + p);
            let back = memchr::memmem::rfind(&data[..anchor], needle)
                .map_or(anchor.min(2048), |p| anchor - p);
            Located {
                span: &data[anchor - back..end],
                anchor,
                back,
                end,
            }
        })
        .collect()
}

/// Verify every block in the file and report coverage.
fn cmd_blocks(path: &str) {
    let mm = map(path);
    if let Ok(h) = header::check(&mm) {
        println!(
            "Nostromo version {:?}{}, page size {}",
            h.version as char,
            if h.known { "" } else { " (untested)" },
            h.page_size
        );
    }
    let offs = block::find_all(&mm);
    let unaligned = block::count_unaligned(&mm);
    let needle = record::anchor_needle();

    let (mut ok, mut bytes, mut with_ipm, mut recs) = (0usize, 0usize, 0usize, 0usize);
    let mut by_kind: BTreeMap<u32, usize> = BTreeMap::new();
    let mut verifier = block::Verifier::new(&mm);

    for &o in &offs {
        let Some(b) = verifier.parse(&mm, o) else {
            continue;
        };
        ok += 1;
        bytes += b.data.len();
        *by_kind.entry(b.kind()).or_default() += 1;
        let n = memchr::memmem::find_iter(&b.data, &needle).count();
        if n > 0 {
            with_ipm += 1;
            recs += n;
        }
    }

    println!("{} block signatures found", offs.len());
    if unaligned > 0 {
        println!(
            "{unaligned} signatures off a {}-byte boundary ignored",
            block::ALIGN
        );
    }
    println!(
        "verified {ok}/{} ({:.2}%), {:.1} MB decompressed",
        offs.len(),
        100.0 * ok as f64 / offs.len() as f64,
        bytes as f64 / 1e6
    );
    println!("{with_ipm} blocks hold mail, {recs} IPM.Note records");
    for (k, n) in &by_kind {
        println!("  type {k:<3} {n}");
    }

    // Second pass: resolve each key to its live revision.
    let s = kv::Store::load(&mm).stats;
    println!(
        "{} page keys, {} system keys, {} stale revisions",
        s.page_keys, s.system_keys, s.stale_revisions
    );
    report_over_budget(&s);
}

/// Say so when the checksum budget left blocks unverified.
fn report_over_budget(s: &kv::Stats) {
    if s.over_budget > 0 {
        println!(
            "budget hit: {} blocks not verified (checksum work over {}x the file size)",
            s.over_budget,
            block::CRC_BUDGET_FACTOR
        );
    }
}

/// Say so when the item scan budget left envelopes out.
fn report_skipped_items(n: usize) {
    if n > 0 {
        println!(
            "budget hit: {n} items skipped (items over {}x the size of their value)",
            item::ITEM_BUDGET_FACTOR
        );
    }
}

/// Print the object type histogram of the live values.
fn cmd_types(path: &str) {
    let mm = map(path);
    let store = kv::Store::load(&mm);
    let hist = inspect::type_histogram(&store, &mm);
    let rows = &hist.rows;
    let total: usize = rows.iter().map(|r| r.count).sum();
    println!(
        "{:<8} {:<40} {:>9}  {:<12}  fixed_len",
        "type", "name", "count", "schema"
    );
    for r in rows {
        let lens: Vec<String> = r.fixed_lens.iter().map(|l| format!("0x{l:x}")).collect();
        println!(
            "0x{:<6x} {:<40} {:>9}  {:<12}  {}",
            r.id,
            r.name.unwrap_or("?"),
            r.count,
            r.schema.as_str(),
            lens.join(",")
        );
    }
    println!("{} types, {total} objects", rows.len());
    report_skipped_items(hist.skipped_items);
    report_over_budget(&store.stats);
}

/// Name of a generic (discovered) field at item offset `at`:
/// `field_<lowercase hex offset>`.
fn field_name(at: usize) -> String {
    format!("field_{at:x}")
}

/// Print the fields of the first `n` objects of type `ty` (name or id).
fn cmd_dump(path: &str, ty: &str, n: usize) {
    let Some(id) = names::type_id(ty) else {
        eprintln!("unknown type {ty:?}: give a type name or a 0x.. / decimal id");
        std::process::exit(2);
    };
    let mm = map(path);
    let store = kv::Store::load(&mm);
    let dump = inspect::dump_objects(&store, &mm, id, n, item::tick_window());
    let objs = &dump.objects;
    let name = names::type_name(id).unwrap_or("?");
    for o in objs {
        println!(
            "\n=== {name} (0x{:x})  page 0x{:x}  +{}  fixed_len 0x{:x}  size {}  {} ===",
            o.type_id,
            o.page,
            o.offset,
            o.fixed_len,
            o.size,
            if o.schema { "schema" } else { "generic" }
        );
        for p in &o.props {
            println!("  {}: {}", p.name, inspect::prop_text(&p.value));
        }
        for f in &o.fields {
            let label = field_name(f.at);
            let more = if f.truncated { "..." } else { "" };
            let shown = match &f.value {
                inspect::Decoded::Utf16(s) | inspect::Decoded::Utf8(s) => format!("{s:?}{more}"),
                inspect::Decoded::Ticks(t) => format!("unix {t}"),
                _ => String::new(),
            };
            println!(
                "  0x{:<5x} {:<14} {:>6}  {:<7} {shown}",
                f.at,
                label,
                f.len,
                f.value.kind()
            );
        }
    }
    println!("\n{} {name} objects shown", objs.len());
    report_skipped_items(dump.skipped_items);
    report_over_budget(&store.stats);
}

/// Print `msg` and exit with status 1.
fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

/// Header of every SQLite database file.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Whether `db_path` exists, after checking that it may be replaced: it must
/// not be the input store, and it must be a SQLite database or an empty file,
/// so a mistyped or swapped argument never destroys another file. A symbolic
/// link, dangling or not, is refused: replacing it would leave the file it
/// names stale.
fn replaceable(input: &str, db_path: &str) -> Result<bool, String> {
    let Ok(meta) = std::fs::symlink_metadata(db_path) else {
        return Ok(false);
    };
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{db_path} is a symbolic link; refusing to replace it (name the real file)"
        ));
    }
    let real = |p: &str| std::fs::canonicalize(p).map_err(|e| format!("{p}: {e}"));
    if real(input)? == real(db_path)? {
        return Err(format!(
            "{db_path} is the input store; refusing to overwrite it"
        ));
    }
    let mut head = Vec::new();
    let sqlite = meta.is_file()
        && File::open(db_path)
            .and_then(|f| f.take(16).read_to_end(&mut head))
            .is_ok()
        && (head.is_empty() || head == SQLITE_MAGIC);
    if !sqlite {
        return Err(format!(
            "{db_path} exists and is not a SQLite database; refusing to overwrite it"
        ));
    }
    Ok(true)
}

/// Remove `db_path` and the side files SQLite may have left next to it.
fn remove_db(db_path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = db_path.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
}

/// The export's work file, `<out>.tmp-<pid>` next to the output. It is removed
/// on drop, so a failed export leaves nothing behind, unless [`TempDb::commit`]
/// has put it in place as the output.
struct TempDb(PathBuf);

impl TempDb {
    /// Create the work file, readable and writable by the owner only: an
    /// export holds mailbox content.
    fn create(db_path: &str) -> std::io::Result<TempDb> {
        let path = PathBuf::from(format!("{db_path}.tmp-{}", std::process::id()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&path)?;
        Ok(TempDb(path))
    }

    /// Put the finished work file in place as `db_path`.
    ///
    /// With `replace`, it is renamed over `db_path`, replacing the database
    /// there and its side files. Without, `db_path` must still not exist: the
    /// work file is hard-linked to it, which fails with `AlreadyExists`
    /// instead of overwriting a file that appeared during the export.
    fn commit(self, db_path: &str, replace: bool) -> std::io::Result<()> {
        let remove_side_files = || {
            for suffix in ["-wal", "-shm", "-journal"] {
                let _ = std::fs::remove_file(format!("{db_path}{suffix}"));
            }
        };
        if replace {
            remove_side_files();
            return std::fs::rename(&self.0, db_path);
        }
        match std::fs::hard_link(&self.0, db_path) {
            // The work file's own name is removed on drop.
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
            // A file system without hard links: rename, unless the output
            // exists by now.
            Err(_) if std::fs::symlink_metadata(db_path).is_err() => {
                std::fs::rename(&self.0, db_path)?;
            }
            Err(e) => return Err(e),
        }
        remove_side_files();
        Ok(())
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        remove_db(&self.0);
    }
}

/// Structural SQLite export to `db_path`.
///
/// The input is opened and checked first, and the export is written to a work
/// file that becomes `db_path` only when it is complete. An existing output
/// is replaced only with `force`, and only when [`replaceable`] allows it;
/// without `force`, an output that appears during the export is kept too.
fn cmd_export(path: &str, db_path: &str, force: bool) {
    let mm = map(path);
    match replaceable(path, db_path) {
        Err(e) => fail(&e),
        Ok(true) if !force => fail(&format!("{db_path} exists; pass --force to overwrite it")),
        Ok(_) => {}
    }
    let store = kv::Store::load(&mm);
    let tmp =
        TempDb::create(db_path).unwrap_or_else(|e| fail(&format!("create {}.tmp-*: {e}", db_path)));
    let result = rusqlite::Connection::open(&tmp.0)
        .map_err(export::ExportError::from)
        .and_then(|db| export::export(&store, &mm, &db, item::tick_window()));
    let st = match result {
        Ok(st) => st,
        Err(e) => {
            drop(tmp);
            fail(&format!("export: {e}"));
        }
    };
    match tmp.commit(db_path, force) {
        Ok(()) => {}
        Err(e) if !force && e.kind() == std::io::ErrorKind::AlreadyExists => fail(&format!(
            "{db_path} appeared during the export; not overwriting it (pass --force to replace it)"
        )),
        Err(e) => fail(&format!("replace {db_path}: {e}")),
    }
    println!(
        "{db_path}: {} values, {} objects, {} duplicates, {} fields, {} collections",
        st.values, st.objects, st.duplicates, st.fields, st.collections
    );
    println!(
        "objects: {} schema ({} superseded copies), {} generic; schema types on the \
         generic path: {} fixed_len mismatches, {} without GUID",
        st.schema_objects, st.superseded, st.generic_objects, st.fixed_len_mismatch, st.no_guid
    );
    report_skipped_items(st.skipped_items);
    if st.skipped_fields > 0 {
        println!(
            "budget hit: {} fields skipped (fields over {}x the heap of their item)",
            st.skipped_fields,
            item::FIELD_BUDGET_FACTOR
        );
    }
    report_over_budget(&store.stats);
}

/// Dump the named field map for the first `n` records.
///
/// With `n = 0`, prints only the aggregate coverage per key.
fn cmd_map(path: &str, n: usize) {
    let mm = map(path);
    let needle = record::anchor_needle();
    let mut maps = Vec::new();
    let mut shown = 0usize;

    for o in block::find_all(&mm) {
        let Some(b) = block::parse(&mm, o) else {
            continue;
        };
        for r in records(&b.data, &needle) {
            let m = schema::build_bounded(&b.data, r.anchor, r.end, r.back);
            if shown < n {
                println!("\n=== block 0x{o:08x}  anchor +{} ===", r.anchor);
                print!("{}", m.dump());
                shown += 1;
            }
            maps.push(m);
        }
    }

    println!("\n{} records\n", maps.len());
    for (k, v) in schema::tally(&maps) {
        println!(
            "  {:<20} {:>6}  {:>5.1}%",
            k,
            v,
            v as f64 * 100.0 / maps.len() as f64
        );
    }
}

/// Show the records whose span contains `term`, with their field maps.
///
/// Used to check a row in the database against the bytes it came from: if a
/// sender and a subject appear in the same map, they really are stored
/// together.
fn cmd_find(path: &str, term: &str, limit: usize) {
    let mm = map(path);
    let needle = record::anchor_needle();
    let want: Vec<u8> = term.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut shown = 0usize;

    for o in block::find_all(&mm) {
        if shown >= limit {
            return;
        }
        let Some(b) = block::parse(&mm, o) else {
            continue;
        };
        // Cheap reject: skip the whole block before walking its records.
        if memchr::memmem::find(&b.data, &want).is_none() {
            continue;
        }
        for r in records(&b.data, &needle) {
            if shown >= limit {
                return;
            }
            if memchr::memmem::find(r.span, &want).is_none() {
                continue;
            }
            shown += 1;
            let m = schema::build_bounded(&b.data, r.anchor, r.end, r.back);
            println!(
                "\n=== block 0x{o:08x}  anchor +{}  span {} bytes ===",
                r.anchor,
                r.span.len()
            );
            print!("{}", m.dump());
        }
    }
    if shown == 0 {
        println!("no records contain {term:?}");
    }
}

/// Parse the whole store into a SQLite database with a full-text index.
fn cmd_db(path: &str, db_path: &str) {
    let mm = map(path);
    // Same guard as `export`: never delete the input or a non-database file.
    if let Err(e) = replaceable(path, db_path) {
        fail(&e);
    }
    let needle = record::anchor_needle();
    let offs = block::find_all(&mm);
    println!("{} blocks -> {db_path}", offs.len());
    // Read the clock once, so every record is judged against one ceiling.
    let tick_hi = item::tick_window().1;

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_path}{suffix}"));
    }
    let mut db = rusqlite::Connection::open(db_path).expect("open db");
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE messages (
             id           INTEGER PRIMARY KEY,
             block        INTEGER,   -- file offset of the source block
             sender       TEXT,
             sender_name  TEXT,
             recipients   TEXT,      -- comma-separated; ordering is not stored
             subject      TEXT,
             message_id   TEXT,
             body         TEXT,      -- plain text, tags stripped
             html         TEXT,      -- original HTML, when the store has it
             body_words   INTEGER,
             html_bytes   INTEGER,
             -- 'full' when the store holds the whole message, 'preview' when it
             -- holds only Outlook's ~255-character cache of it. Most messages
             -- are preview-only: that is the format, not a parse failure.
             body_kind    TEXT,
             sent_unix    INTEGER,   -- earliest .NET tick in the record span
             sent_utc     TEXT,
             -- 1 when the subject came from another record in the same
             -- conversation, because this record stores none of its own.
             subject_inherited INTEGER
         );
         CREATE INDEX messages_sent ON messages(sent_unix);
         CREATE INDEX messages_sender ON messages(sender);
         CREATE VIRTUAL TABLE messages_fts USING fts5(
             sender, sender_name, recipients, subject, body,
             content='messages', content_rowid='id', tokenize='porter unicode61'
         );",
    )
    .expect("schema");

    let mut blocks_ok = 0usize;
    let mut best: std::collections::HashMap<String, record::Record> = Default::default();
    // `Topic` is a conversation-level field, so a record can legitimately hold
    // no subject of its own while its thread has one. These map each thread to
    // its known subject, and each message identity to its threads.
    let mut convo_subject: std::collections::HashMap<String, String> = Default::default();
    let mut convo_of: std::collections::HashMap<String, Vec<String>> = Default::default();

    for &o in &offs {
        let Some(b) = block::parse(&mm, o) else {
            continue;
        };
        blocks_ok += 1;

        for loc in records(&b.data, &needle) {
            let mut r = record::parse(o, &b.data, loc.anchor);
            let m = schema::build_bounded(&b.data, loc.anchor, loc.end, loc.back);

            // The named map is authoritative for every field it covers: it
            // assigns slots by their position in the run sequence and checks
            // each against the type the protocol schema says it holds.
            // `record::parse` supplies only what is not a string run -- the
            // HTML body -- plus the send time, read here from the span.
            r.sent_unix = record::read_time(loc.span, tick_hi);
            // Take the map's value where it has one, but keep what
            // `record::parse` found otherwise -- the two paths locate fields
            // differently, and the shape-based path still covers records the
            // map's stricter type checks reject.
            if let Some(v) = m.get(schema::Key::SenderAddress) {
                r.sender = Some(v.to_string());
            }
            if let Some(v) = m.get(schema::Key::SenderName) {
                r.sender_name = Some(v.to_string());
            }
            if let Some(v) = m.get(schema::Key::InternetMessageId) {
                r.message_id = Some(v.to_string());
            }
            // The subject is the exception: the map is the only path that
            // requires the Topic/NormalizedSubject duplicate, and the
            // shape-based fallback supplies attachment paths instead of
            // subjects, so an empty map result must win.
            r.subject = m.get(schema::Key::NormalizedSubject).map(str::to_string);
            r.recipients = m
                .all(schema::Key::Recipient)
                .iter()
                .filter(|t| Some(**t) != r.sender.as_deref())
                .map(|t| t.to_string())
                .collect();
            if r.body.split_whitespace().count() < 3 {
                if let Some(v) = m.get(schema::Key::Preview) {
                    r.body = v.to_string();
                }
            }

            // Built from every raw record, not the deduplicated set: a revision
            // that loses the merge can still be the only one carrying the
            // thread's subject.
            let convo_ids: Vec<String> = m
                .all(schema::Key::ConversationId)
                .iter()
                .map(|s| s.to_string())
                .collect();

            // A record with neither a sender nor readable text is a stub.
            if r.sender.is_none() && r.body.split_whitespace().count() < 5 {
                continue;
            }

            // Identity is sender + send time: the only fields present on
            // essentially every record. `InternetMessageId` is deliberately
            // excluded -- the store reuses one across a conversation, so keying
            // on it merges distinct messages, and pairing it with anything else
            // splits the revisions of one. See SPEC.md, "Legacy commands".
            let key = format!("{:?}|{:?}", r.sender, r.sent_unix);
            for id in &convo_ids {
                if let Some(sub) = &r.subject {
                    convo_subject
                        .entry(id.clone())
                        .or_insert_with(|| sub.clone());
                }
            }
            let slot = convo_of.entry(key.clone()).or_default();
            for id in convo_ids {
                if !slot.contains(&id) {
                    slot.push(id);
                }
            }

            match best.get_mut(&key) {
                None => {
                    best.insert(key, r);
                }
                // Merge field by field rather than keeping one whole record.
                // Revisions are not uniformly complete -- one carries the
                // subject, another the full HTML body -- so choosing a single
                // "best" record discards fields recovered from a sibling.
                Some(prev) => merge(prev, r),
            }
        }
    }

    // Back-fill subjects from the conversation. Measured over the store, 90.9 %
    // of *records* lacking a subject share a thread identifier with one that
    // has it; most of those merge into messages that already recovered a
    // subject, leaving 219 messages genuinely fixed by this pass.
    let mut backfilled = 0usize;
    for (key, rec) in best.iter_mut() {
        if rec.subject.is_some() {
            continue;
        }
        let Some(ids) = convo_of.get(key) else {
            continue;
        };
        if let Some(sub) = ids.iter().find_map(|i| convo_subject.get(i)) {
            rec.subject = Some(sub.clone());
            rec.subject_inherited = true;
            backfilled += 1;
        }
    }
    println!("  {backfilled} subjects recovered from conversation threads");

    // A display name equal to the subject is the subject. Extraction rejects
    // this within a record, but a merged message can take its name from one
    // revision and its subject from another, so the check runs again here.
    for r in best.values_mut() {
        if r.sender_name.is_some() && r.sender_name == r.subject {
            r.sender_name = None;
        }
    }

    let stored = best.len();
    let tx = db.transaction().expect("tx");
    {
        let mut ins = tx
            .prepare(
                "INSERT INTO messages
                   (block,sender,sender_name,recipients,subject,message_id,body,
                    html,body_words,html_bytes,body_kind,sent_unix,sent_utc,
                    subject_inherited)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,
                         datetime(?12,'unixepoch'),?13)",
            )
            .expect("prepare");

        for r in best.values() {
            ins.execute(rusqlite::params![
                r.offset as i64,
                r.sender,
                r.sender_name,
                (!r.recipients.is_empty()).then(|| r.recipients.join(", ")),
                r.subject,
                r.message_id,
                r.body,
                r.html,
                r.body.split_whitespace().count() as i64,
                r.html.as_ref().map(|h| h.len() as i64),
                if r.html.is_some() { "full" } else { "preview" },
                r.sent_unix,
                r.subject_inherited as i64,
            ])
            .expect("insert");
        }
    }
    tx.commit().expect("commit");

    db.execute_batch(
        "INSERT INTO messages_fts(rowid,sender,sender_name,recipients,subject,body)
           SELECT id,sender,sender_name,recipients,subject,body FROM messages;
         INSERT INTO messages_fts(messages_fts) VALUES('optimize');
         VACUUM;",
    )
    .expect("index");

    println!(
        "\n{blocks_ok}/{} blocks verified, {stored} messages stored",
        offs.len()
    );
    for (label, sql) in [
        ("sender     ", "sender IS NOT NULL"),
        ("date       ", "sent_unix IS NOT NULL"),
        ("sender name", "sender_name IS NOT NULL"),
        ("subject    ", "subject IS NOT NULL"),
        ("message-id ", "message_id IS NOT NULL"),
        ("body       ", "body_words > 3"),
        ("full HTML  ", "body_kind = 'full'"),
    ] {
        let n: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM messages WHERE {sql}"),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        println!(
            "  {label} {n:>6}  {:>5.1}%",
            n as f64 * 100.0 / stored as f64
        );
    }
}

/// Combine a later revision of a message into the copy already held.
fn merge(prev: &mut record::Record, r: record::Record) {
    if prev.subject.is_none() {
        prev.subject = r.subject;
    }
    if prev.sender_name.is_none() {
        prev.sender_name = r.sender_name;
    }
    if prev.message_id.is_none() {
        prev.message_id = r.message_id;
    }
    if r.recipients.len() > prev.recipients.len() {
        prev.recipients = r.recipients;
    }
    let better_html = match (&prev.html, &r.html) {
        (None, Some(_)) => true,
        (Some(a), Some(b)) => b.len() > a.len(),
        _ => false,
    };
    if better_html {
        prev.html = r.html;
    }
    if r.body.len() > prev.body.len() {
        prev.body = r.body;
    }
}

const USAGE: &str = "\
hxprobe — parse HxStore.hxd, the New Outlook for Mac message store

usage:
  hxprobe blocks <file>              verify every block, report coverage
  hxprobe types  <file>              object type histogram: id, name, count, schema, fixed_len
  hxprobe dump   <file> <type> [n]   fields of n objects of a type (name or id, default 5)
  hxprobe export <file> [out.db] [--force]
                                     structural SQLite export (default hxstore.db);
                                     --force replaces an existing SQLite output
  hxprobe db    <file> [out.db]     build a SQLite database (default mail.db)
  hxprobe map    <file> [n]          dump the field map for n records (0 = stats)
  hxprobe find   <file> <term> [n]   show records whose text contains <term>
  hxprobe mcp [--store <file>] [--cache-dir <dir>] [--refresh-secs <n>]
              [--idle-secs <n>] [--no-service]
                                     read-only MCP server on stdin/stdout; --store
                                     defaults to the Outlook profile's HxStore.hxd,
                                     --cache-dir to ~/Library/Caches/hxprobe-mcp,
                                     --refresh-secs (shortest time between two
                                     rebuilds of a changed store) to 900; the
                                     cache is built by the service of the cache
                                     directory, started when none runs (with
                                     --refresh-secs and --idle-secs); with
                                     --no-service or HXPROBE_SERVICE=0 the
                                     server builds it itself
  hxprobe service [--store <file>] [--cache-dir <dir>] [--refresh-secs <n>]
                  [--idle-secs <n>]
                                     the one process per cache directory that
                                     builds the MCP cache; runs without a
                                     terminal and exits after --idle-secs
                                     (default 1800, 0 = never) without requests
  hxprobe service --status [--cache-dir <dir>]
                                     print the status of the running service as
                                     one JSON line; exit 1 when none answers
  hxprobe service --stop [--cache-dir <dir>]
                                     stop the running service (any version) and
                                     wait until it is gone
  hxprobe --version                  print the version

Work on a snapshot, not the live store: Outlook holds HxStore.lock and
rewrites the file while it runs.";

/// Options of `hxprobe mcp`; `None` where not given.
#[derive(Debug, Default)]
struct McpOptions {
    store: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    refresh_secs: Option<u64>,
    idle_secs: Option<u64>,
    /// `--no-service`: build the cache in this process.
    no_service: bool,
}

/// Parse the options after `mcp`; a usage error message on failure.
fn parse_mcp_options(args: &[String]) -> Result<McpOptions, String> {
    let mut o = McpOptions::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--store" => o.store = Some(PathBuf::from(value()?)),
            "--cache-dir" => o.cache_dir = Some(PathBuf::from(value()?)),
            "--refresh-secs" | "--idle-secs" => {
                let v = value()?;
                let n = v
                    .parse()
                    .map_err(|_| format!("{a}: {v:?} is not a number"))?;
                if a == "--refresh-secs" {
                    o.refresh_secs = Some(n);
                } else {
                    o.idle_secs = Some(n);
                }
            }
            "--no-service" => o.no_service = true,
            other => return Err(format!("unknown mcp option {other:?}")),
        }
    }
    Ok(o)
}

/// `hxprobe mcp`: serve the MCP tools on stdin/stdout until end of input.
/// stdout carries only JSON-RPC messages; logs go to stderr.
///
/// Service mode (the default): the refresh service of the cache directory
/// builds the cache; it is started from this program's own file when none
/// answers. Fallback mode, where this process builds the cache itself:
/// `--no-service`, `HXPROBE_SERVICE=0`, or a socket path that does not fit.
fn cmd_mcp(args: &[String]) {
    use hxprobe::mcp::{cache, client, log, proto, rpc};
    use std::io::Write;

    // Children (the service) must stay waitable for their exit status,
    // also when the client started this server with SIGCHLD ignored.
    // SAFETY: no thread exists yet; signal() has no other preconditions.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };

    // A panic message could hold mail content: print only where it happened.
    // Nothing here uses `eprintln!`, which panics when stderr is closed.
    std::panic::set_hook(Box::new(|info| match info.location() {
        Some(l) => log(&format!("internal error at {}:{}", l.file(), l.line())),
        None => log("internal error"),
    }));
    let fail = |msg: String| -> ! {
        log(&msg);
        std::process::exit(1);
    };
    let opts = parse_mcp_options(args).unwrap_or_else(|e| {
        let _ = writeln!(std::io::stderr(), "{e}\n\n{USAGE}");
        std::process::exit(2);
    });
    // A store that cannot be examined for lack of permission is served
    // anyway, so that tool calls can say what to do; the path is resolved
    // again at every call until the access is granted.
    let (store, denied_default) = match opts.store {
        Some(s) => (s, false),
        None => match cache::find_default_store() {
            cache::StoreLookup::Found(p) => (p, false),
            cache::StoreLookup::Denied(p) => (p, true),
            cache::StoreLookup::Missing => fail(cache::NO_DEFAULT_STORE.into()),
        },
    };
    let examined = if denied_default {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    } else {
        std::fs::metadata(&store)
    };
    match examined {
        Ok(m) if m.is_file() => check_store_header(&store),
        // The guess may name no file: the denial may be of another profile.
        Err(_) if denied_default => log(&format!(
            "the default store cannot be looked up: permission denied; serving anyway, \
             guessing {}",
            store.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => log(&format!(
            "{} cannot be examined: permission denied; serving anyway",
            store.display()
        )),
        _ => fail(format!("{} is not a store file", store.display())),
    }
    let cache_dir = opts
        .cache_dir
        .or_else(|| env::var_os("HOME").map(|h| Path::new(&h).join("Library/Caches/hxprobe-mcp")))
        .unwrap_or_else(|| fail("HOME is not set; pass --cache-dir".into()));
    let refresh_secs = opts.refresh_secs.unwrap_or(cache::DEFAULT_REFRESH_SECS);
    let remedy = install_remedy();
    let mut cache = cache::Cache::new(&cache_dir, &store)
        .unwrap_or_else(|e| fail(format!("cache directory {}: {e}", cache_dir.display())))
        .with_refresh_secs(refresh_secs)
        .with_remedy(remedy.clone());
    if denied_default {
        cache = cache.with_denied_default();
    }
    // PDF text is made by this program's own file, started as `pdf-text`.
    if let Ok(program) = client::service_program() {
        cache = cache.with_text_helper(program);
    }
    log(&format!(
        "serving {} with cache {}",
        cache.store_path().display(),
        cache_dir.display()
    ));
    let service = if opts.no_service {
        Err("--no-service".to_string())
    } else if env::var_os("HXPROBE_SERVICE").is_some_and(|v| v == "0") {
        Err("HXPROBE_SERVICE=0".to_string())
    } else {
        client::service_program().and_then(|program| {
            client::Client::new(
                &cache_dir,
                cache.store_path(),
                program,
                refresh_secs,
                opts.idle_secs.unwrap_or(proto::DEFAULT_IDLE_SECS),
            )
            .map(|c| c.with_remedy(remedy.clone()))
        })
    };
    match service {
        Ok(c) => {
            log("service mode: the refresh service builds the cache");
            cache = cache.with_refresher(Box::new(c));
        }
        Err(why) => log(&format!(
            "fallback mode, this server builds the cache: {why}"
        )),
    }
    let mut server = rpc::Server::new(cache);
    if let Err(e) = rpc::run(
        &mut server,
        std::io::stdin().lock(),
        std::io::stdout().lock(),
    ) {
        fail(format!("i/o error: {e}"));
    }
}

/// Log a warning when the header of `store` is not one of a known store
/// version. Only the header is read: the store itself is read from a
/// private copy.
fn check_store_header(store: &Path) {
    use hxprobe::mcp::log;
    let mut head = Vec::with_capacity(0x40);
    let header = File::open(store)
        .and_then(|f| f.take(0x40).read_to_end(&mut head))
        .map_err(|e| e.to_string())
        .and_then(|_| header::check(&head));
    match header {
        Err(e) => log(&format!("warning: {}: {e}", store.display())),
        Ok(h) if !h.known => log(&format!(
            "warning: {}: store version {:?} (0x{:02x}) is untested",
            store.display(),
            h.version as char,
            h.version
        )),
        Ok(_) => {}
    }
}

/// What a denial text names: this program's own file and the note of the
/// installer in `HXPROBE_INSTALL_NOTE` (see [`proto::denied_text`]).
fn install_remedy() -> hxprobe::mcp::proto::Remedy {
    hxprobe::mcp::proto::Remedy {
        program: hxprobe::mcp::client::service_program().ok(),
        note: env::var("HXPROBE_INSTALL_NOTE")
            .ok()
            .filter(|n| !n.is_empty()),
    }
}

/// What `hxprobe service` does.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ServiceAction {
    /// Be the service.
    #[default]
    Run,
    /// `--status`: print the running service's status.
    Status,
    /// `--stop`: stop the running service.
    Stop,
}

/// Options of `hxprobe service`; `None` where not given.
#[derive(Debug, Default)]
struct ServiceOptions {
    action: ServiceAction,
    store: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    refresh_secs: Option<u64>,
    idle_secs: Option<u64>,
}

/// Parse the options after `service`; a usage error message on failure.
fn parse_service_options(args: &[String]) -> Result<ServiceOptions, String> {
    let mut o = ServiceOptions::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--store" => o.store = Some(PathBuf::from(value()?)),
            "--cache-dir" => o.cache_dir = Some(PathBuf::from(value()?)),
            "--refresh-secs" | "--idle-secs" => {
                let v = value()?;
                let n = v
                    .parse()
                    .map_err(|_| format!("{a}: {v:?} is not a number"))?;
                if a == "--refresh-secs" {
                    o.refresh_secs = Some(n);
                } else {
                    o.idle_secs = Some(n);
                }
            }
            "--status" | "--stop" => {
                if o.action != ServiceAction::Run {
                    return Err("--status and --stop exclude each other".into());
                }
                o.action = if a == "--status" {
                    ServiceAction::Status
                } else {
                    ServiceAction::Stop
                };
            }
            other => return Err(format!("unknown service option {other:?}")),
        }
    }
    if o.action != ServiceAction::Run
        && (o.store.is_some() || o.refresh_secs.is_some() || o.idle_secs.is_some())
    {
        return Err("--status and --stop take only --cache-dir".into());
    }
    Ok(o)
}

/// The cache directory of `hxprobe mcp` and `hxprobe service`: `given`,
/// else `~/Library/Caches/hxprobe-mcp`; `None` without `HOME`.
fn cache_dir_or_default(given: Option<PathBuf>) -> Option<PathBuf> {
    given.or_else(|| env::var_os("HOME").map(|h| Path::new(&h).join("Library/Caches/hxprobe-mcp")))
}

/// `hxprobe service --status` and `--stop`: messages on stderr, the
/// status on stdout; exit 0 on success, 1 otherwise.
fn cmd_service_client(action: ServiceAction, cache_dir: Option<PathBuf>) -> ! {
    use hxprobe::mcp::service;
    use std::io::Write;
    let fail = |msg: &str| -> ! {
        let _ = writeln!(std::io::stderr(), "hxprobe service: {msg}");
        std::process::exit(1);
    };
    let Some(dir) = cache_dir_or_default(cache_dir) else {
        fail("HOME is not set; pass --cache-dir")
    };
    match action {
        ServiceAction::Status => match service::status_command(&dir) {
            Ok(line) => {
                if writeln!(std::io::stdout(), "{line}").is_err() {
                    std::process::exit(1);
                }
            }
            Err(e) => fail(&e),
        },
        ServiceAction::Stop => match service::stop_command(&dir) {
            Ok(msg) => {
                let _ = writeln!(std::io::stderr(), "hxprobe service: {msg}");
            }
            Err(e) => fail(&e),
        },
        ServiceAction::Run => unreachable!("not a client command"),
    }
    std::process::exit(0);
}

/// Block `SIGTERM` and `SIGINT` (with their default action restored, as an
/// inherited "ignore" would discard them) and ignore `SIGHUP`. Called
/// before any thread exists, so every thread inherits the mask and only
/// [`spawn_signal_thread`] receives the two signals.
fn block_stop_signals() -> libc::sigset_t {
    // SAFETY: plain system calls on a local signal set; no thread exists
    // yet whose signal handling could change meanwhile.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

/// Wait for one of the signals in `set` on a thread of its own, then ask
/// the service to stop.
fn spawn_signal_thread(set: libc::sigset_t, stop: hxprobe::mcp::service::Stop) {
    use hxprobe::mcp::{log, service::Exit};
    let started = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || loop {
            let mut sig: libc::c_int = 0;
            // SAFETY: `set` and `sig` are valid for the call.
            if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
                log(&format!("signal {sig} received"));
                stop.request(Exit::Signal);
                return;
            }
        });
    if let Err(e) = started {
        log(&format!("cannot wait for signals: {e}"));
    }
}

/// `hxprobe service`: build and refresh the MCP cache of one directory for
/// the MCP servers that use it, until idle, a `shutdown` request, a signal,
/// or the removal of its socket. Exit status 0 then; 1 when it cannot
/// start, 2 on a usage error, 75 when another service holds the
/// directory's `service.lock`, 77 when the store (or the default profile
/// directory) cannot be examined or read for lack of permission. With
/// `--status` or `--stop`, see [`cmd_service_client`].
fn cmd_service(args: &[String]) {
    use hxprobe::mcp::service::{self, CacheBackend, Core, InstanceLock, LockError, Stop};
    use hxprobe::mcp::{self, cache, log, proto};
    use std::io::Write;
    use std::sync::Arc;

    let opts = parse_service_options(args).unwrap_or_else(|e| {
        let _ = writeln!(std::io::stderr(), "{e}\n\n{USAGE}");
        std::process::exit(2);
    });
    if opts.action != ServiceAction::Run {
        cmd_service_client(opts.action, opts.cache_dir);
    }
    mcp::set_log_style(mcp::LogStyle::Service);
    // A panic message could hold mail content: print only where it happened.
    std::panic::set_hook(Box::new(|info| match info.location() {
        Some(l) => log(&format!("internal error at {}:{}", l.file(), l.line())),
        None => log("internal error"),
    }));
    // SAFETY: umask() has no preconditions and cannot fail.
    unsafe { libc::umask(0o077) };
    let signals = block_stop_signals();
    let fail = |msg: String| -> ! {
        log(&msg);
        std::process::exit(1);
    };
    // A store that cannot be examined for lack of permission is the same
    // exit as one that cannot be read (see below).
    let denied = |store: &Path| -> ! {
        log(&format!(
            "{} cannot be examined: permission denied; exiting",
            store.display()
        ));
        std::process::exit(service::EXIT_PERMISSION);
    };
    let is_denied = |e: &std::io::Error| e.kind() == std::io::ErrorKind::PermissionDenied;
    let store = match opts.store {
        Some(s) => s,
        None => match cache::find_default_store() {
            cache::StoreLookup::Found(p) => p,
            cache::StoreLookup::Denied(p) => denied(&p),
            cache::StoreLookup::Missing => fail(cache::NO_DEFAULT_STORE.into()),
        },
    };
    match std::fs::metadata(&store) {
        Ok(m) if m.is_file() => {}
        Err(e) if is_denied(&e) => denied(&store),
        _ => fail(format!("{} is not a store file", store.display())),
    }
    let store = std::fs::canonicalize(&store).unwrap_or_else(|e| {
        if is_denied(&e) {
            denied(&store)
        }
        fail(format!("{}: {e}", store.display()))
    });
    let dir = cache_dir_or_default(opts.cache_dir)
        .unwrap_or_else(|| fail("HOME is not set; pass --cache-dir".into()));
    let dir = std::path::absolute(&dir).unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())));
    cache::ensure_dir(&dir).unwrap_or_else(|e| fail(e));
    let sock = proto::socket_path(&dir).unwrap_or_else(|e| fail(e));

    let _lock = match InstanceLock::acquire(&dir) {
        Ok(l) => l,
        Err(LockError::Busy) => {
            log("another service holds the lock; exiting");
            std::process::exit(service::EXIT_BUSY);
        }
        Err(LockError::Failed(e)) => fail(e),
    };
    let backend = CacheBackend::new(&dir, &store);
    // A test aid, not in a release build: pause every build after the
    // store copy, so a test can stop the service in the middle of a build.
    #[cfg(debug_assertions)]
    let backend = match env::var("HXPROBE_TEST_BUILD_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(ms) => {
            log(&format!("test build delay {ms} ms"));
            backend.with_build_delay(std::time::Duration::from_millis(ms))
        }
        None => backend,
    };
    if service::Backend::store_denied(&backend) {
        log(&format!(
            "{} cannot be read: permission denied; exiting",
            store.display()
        ));
        std::process::exit(service::EXIT_PERMISSION);
    }
    if !cache::remove_dead_leftovers(&dir) {
        log("another process is building; temporary build files kept");
    }
    let refresh_secs = opts.refresh_secs.unwrap_or(cache::DEFAULT_REFRESH_SECS);
    let idle_secs = opts.idle_secs.unwrap_or(proto::DEFAULT_IDLE_SECS);
    log(&format!(
        "version {}, store {}, cache {}, refresh {refresh_secs} s, idle {idle_secs} s",
        proto::VERSION,
        store.display(),
        dir.display()
    ));
    let listener = service::bind_socket(&sock)
        .unwrap_or_else(|e| fail(format!("bind {}: {e}", sock.display())));
    let socket = service::SocketFile::of(&listener)
        .unwrap_or_else(|e| fail(format!("{}: {e}", sock.display())));
    log(&format!("listening on {}", sock.display()));

    let stop = Stop::new();
    spawn_signal_thread(signals, stop.clone());
    let clock: service::Clock = Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    });
    let core = Core::new(
        backend,
        store.to_string_lossy().into_owned(),
        refresh_secs,
        idle_secs,
        clock(),
    )
    .with_denied_text(proto::denied_text(
        proto::Denied::Service,
        &install_remedy(),
        &dir,
    ));
    let exit = service::serve(core, listener, &socket, clock, proto::TICK, stop);
    // The socket goes first; `service.lock` is released when the process
    // ends. A build still running is abandoned: its temporary files go
    // (the build lock it holds is released with the process).
    socket.remove_if_ours();
    cache::remove_own_temps(&dir);
    log(&format!("exiting: {}", exit.as_str()));
    std::process::exit(exit.code());
}

/// `hxprobe pdf-text` (hidden, not in the usage text): the text of the PDF
/// on stdin, written to stdout; started by `hxprobe mcp` for one PDF
/// attachment (`mcp/pdf.rs`), so that the PDF library never runs inside
/// the server.
///
/// It takes no argument (`extra` of them: exit 2, nothing read), reads only
/// its stdin (more than [`hxprobe::mcp::attach::MAX_SOURCE_BYTES`]: exit
/// 3), opens no file other than `/dev/null`, reads no environment variable
/// and prints nothing to stderr. Before the library is touched, descriptor
/// 1 is duplicated for the result and pointed at `/dev/null`, so nothing
/// the library prints can reach the result. Text: exit 0; an error, a
/// panic or empty input: exit 1 and nothing written.
fn cmd_pdf_text(extra: usize) -> ! {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::process::exit;

    if extra > 0 {
        exit(2);
    }
    // A panic message could hold content of the file: print nothing.
    std::panic::set_hook(Box::new(|_| {}));
    // SAFETY: fcntl() duplicates descriptor 1 into a new descriptor of at
    // least 3, close-on-exec; no other thread exists yet.
    let result_fd = unsafe { libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3) };
    if result_fd < 0 {
        exit(1);
    }
    // SAFETY: open() of a constant, NUL-terminated path; dup2() replaces
    // descriptor 1 with it; close() of the descriptor open() returned, which
    // nothing else owns. No other thread exists yet.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if null < 0 || libc::dup2(null, 1) < 0 {
            exit(1);
        }
        libc::close(null);
    }
    // SAFETY: `result_fd` is a fresh descriptor owned by nothing else.
    let mut result = unsafe { File::from_raw_fd(result_fd) };

    let max = hxprobe::mcp::attach::MAX_SOURCE_BYTES;
    let mut input = Vec::new();
    if std::io::stdin()
        .lock()
        .take(max + 1)
        .read_to_end(&mut input)
        .is_err()
    {
        exit(1);
    }
    if input.len() as u64 > max {
        exit(3);
    }
    if input.is_empty() {
        exit(1);
    }
    // A deep document may need a deep stack; a panic is caught, an abort
    // ends the process, which the server sees as no text.
    let text = std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&input).ok())
                .ok()
                .flatten()
        })
        .ok()
        .and_then(|h| h.join().ok())
        .flatten();
    match text {
        Some(t) if result.write_all(t.as_bytes()).is_ok() => exit(0),
        _ => exit(1),
    }
}

fn main() {
    // `pdf-text` before any other work, from the raw arguments (one that
    // is not UTF-8 is still an argument, refused).
    let mut raw = env::args_os();
    if raw.nth(1).as_deref() == Some(std::ffi::OsStr::new("pdf-text")) {
        cmd_pdf_text(raw.count());
    }
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--version") {
        println!("hxprobe {}", hxprobe::mcp::proto::VERSION);
        return;
    }
    // `service` takes only options, all optional.
    if args.get(1).map(String::as_str) == Some("service") {
        cmd_service(&args[2..]);
        return;
    }
    // `mcp` takes only options, all optional.
    if args.get(1).map(String::as_str) == Some("mcp") {
        cmd_mcp(&args[2..]);
        return;
    }
    // `export` accepts `--force` anywhere after the command.
    if args.get(1).map(String::as_str) == Some("export") {
        let force = args[2..].iter().any(|a| a == "--force");
        let pos: Vec<&str> = args[2..]
            .iter()
            .map(String::as_str)
            .filter(|a| *a != "--force")
            .collect();
        let Some(file) = pos.first() else {
            eprintln!("export needs a store file\n\n{USAGE}");
            std::process::exit(2);
        };
        cmd_export(file, pos.get(1).copied().unwrap_or("hxstore.db"), force);
        return;
    }
    if args.len() < 3 {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let num = |i: usize, d: usize| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    // Like `num`, but a count that is not a number is a usage error.
    let strict_num = |i: usize, d: usize| match args.get(i).map(|s| s.parse()) {
        None => d,
        Some(Ok(n)) => n,
        Some(Err(_)) => {
            eprintln!("{:?} is not a number\n\n{USAGE}", args[i]);
            std::process::exit(2);
        }
    };

    match args[1].as_str() {
        "blocks" => cmd_blocks(&args[2]),
        "types" => cmd_types(&args[2]),
        "dump" => match args.get(3) {
            Some(ty) => cmd_dump(&args[2], ty, strict_num(4, 5)),
            None => {
                eprintln!("dump needs a type\n\n{USAGE}");
                std::process::exit(2);
            }
        },
        "db" => cmd_db(
            &args[2],
            args.get(3).map(String::as_str).unwrap_or("mail.db"),
        ),
        "map" => cmd_map(&args[2], num(3, 4)),
        "find" => match args.get(3) {
            Some(term) => cmd_find(&args[2], term, num(4, 4)),
            None => {
                eprintln!("find needs a search term\n\n{USAGE}");
                std::process::exit(2);
            }
        },
        other => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let p = env::temp_dir().join(format!("hxprobe-main-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }

        /// Names of the files in the directory, sorted.
        fn files(&self) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            v.sort();
            v
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A work file for `out` holding `content`.
    fn work_file(out: &str, content: &[u8]) -> TempDb {
        let tmp = TempDb::create(out).unwrap();
        std::fs::write(&tmp.0, content).unwrap();
        tmp
    }

    #[test]
    fn generic_field_name_is_hex_offset() {
        assert_eq!(field_name(0x1a), "field_1a");
        assert_eq!(field_name(0x4d4), "field_4d4");
    }

    #[test]
    fn commit_without_replace_keeps_an_output_that_appeared() {
        let d = Dir::new("appeared");
        let out = d.0.join("out.db");
        let out = out.to_str().unwrap();
        let tmp = work_file(out, b"new export");
        // Another writer creates the output, and a journal, during the export.
        std::fs::write(out, b"someone else").unwrap();
        std::fs::write(format!("{out}-journal"), b"theirs").unwrap();

        let e = tmp.commit(out, false).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(out).unwrap(), b"someone else");
        // The work file is gone and the other writer's files are untouched.
        assert_eq!(d.files(), ["out.db", "out.db-journal"]);
    }

    #[test]
    fn commit_without_replace_creates_the_output() {
        let d = Dir::new("create");
        let out = d.0.join("out.db");
        let out = out.to_str().unwrap();
        // A side file left by a database that was deleted earlier.
        std::fs::write(format!("{out}-wal"), b"stale").unwrap();
        work_file(out, b"new export").commit(out, false).unwrap();
        assert_eq!(std::fs::read(out).unwrap(), b"new export");
        assert_eq!(d.files(), ["out.db"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(out).unwrap();
            assert_eq!((meta.mode() & 0o777, meta.nlink()), (0o600, 1));
        }
    }

    #[test]
    fn commit_with_replace_overwrites_the_output() {
        let d = Dir::new("replace");
        let out = d.0.join("out.db");
        let out = out.to_str().unwrap();
        let tmp = work_file(out, b"new export");
        std::fs::write(out, b"old export").unwrap();
        std::fs::write(format!("{out}-journal"), b"stale").unwrap();
        tmp.commit(out, true).unwrap();
        assert_eq!(std::fs::read(out).unwrap(), b"new export");
        assert_eq!(d.files(), ["out.db"]);
    }
}
