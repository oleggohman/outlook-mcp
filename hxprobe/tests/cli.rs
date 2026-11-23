//! Command-line behaviour of the `hxprobe` binary around its output file.
//!
//! Every test works in its own directory under the system temp dir, on
//! synthetic files only, and removes the directory when done.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A scratch directory removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("hxprobe-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
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

/// A store with a valid header and no blocks.
fn empty_store() -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[..8].copy_from_slice(hxprobe::header::MAGIC);
    s[8] = b'i';
    s[0x38..0x40].copy_from_slice(&4096u64.to_le_bytes());
    s
}

/// The store of `testutil::overlapping_blocks`, which the integration tests
/// cannot reach: `n` block headers with a valid header checksum, each claiming
/// a value that runs to the end of the file.
fn overlapping_blocks(n: usize) -> Vec<u8> {
    let mut s = empty_store();
    s.resize(512 + n * 512, 0);
    let len = s.len();
    for off in (512..len).step_by(512) {
        let val_len = (len - off - 0x28) as u32;
        s[off + 0x08..off + 0x10].copy_from_slice(&hxprobe::block::MAGIC.to_le_bytes());
        s[off + 0x10..off + 0x14].copy_from_slice(&8u32.to_le_bytes());
        s[off + 0x14..off + 0x18].copy_from_slice(&val_len.to_le_bytes());
        s[off + 0x18..off + 0x1c].copy_from_slice(&1u32.to_le_bytes());
        s[off + 0x1c..off + 0x20].copy_from_slice(&4u32.to_le_bytes());
        let head = hxprobe::block::crc32(&s[off + 0x04..off + 0x20]);
        s[off..off + 4].copy_from_slice(&head.to_le_bytes());
    }
    s
}

fn hxprobe(args: &[&Path], flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hxprobe"))
        .args(args)
        .args(flags)
        .output()
        .unwrap()
}

fn export(input: &Path, out: &Path, flags: &[&str]) -> Output {
    hxprobe(&[Path::new("export"), input, out], flags)
}

fn is_sqlite(p: &Path) -> bool {
    std::fs::read(p).unwrap().starts_with(b"SQLite format 3\0")
}

#[test]
fn export_writes_a_private_database_and_no_work_file() {
    let d = Dir::new("fresh");
    let (snap, out) = (d.path("snap.hxd"), d.path("out.db"));
    std::fs::write(&snap, empty_store()).unwrap();
    let o = export(&snap, &out, &[]);
    assert!(o.status.success(), "{o:?}");
    assert!(is_sqlite(&out));
    assert_eq!(d.files(), ["out.db", "snap.hxd"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&out).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn export_needs_force_and_then_replaces_a_database() {
    let d = Dir::new("force");
    let (snap, out) = (d.path("snap.hxd"), d.path("out.db"));
    std::fs::write(&snap, empty_store()).unwrap();
    assert!(export(&snap, &out, &[]).status.success());
    // A stale side file of the old database must not survive the new one.
    std::fs::write(d.path("out.db-journal"), b"stale").unwrap();

    let o = export(&snap, &out, &[]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(String::from_utf8_lossy(&o.stderr).contains("--force"));

    let o = export(&snap, &out, &["--force"]);
    assert!(o.status.success(), "{o:?}");
    assert!(is_sqlite(&out));
    assert_eq!(d.files(), ["out.db", "snap.hxd"]);
}

#[test]
fn export_onto_its_own_input_keeps_the_input() {
    let d = Dir::new("same");
    let snap = d.path("snap.hxd");
    std::fs::write(&snap, empty_store()).unwrap();
    let o = export(&snap, &snap, &["--force"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(String::from_utf8_lossy(&o.stderr).contains("input store"));
    assert_eq!(std::fs::read(&snap).unwrap(), empty_store());
    assert_eq!(d.files(), ["snap.hxd"]);
}

#[test]
fn export_with_swapped_arguments_keeps_the_store() {
    let d = Dir::new("swapped");
    let (old_db, keep) = (d.path("old.db"), d.path("keep.hxd"));
    std::fs::write(&old_db, b"SQLite format 3\0 and then some").unwrap();
    std::fs::write(&keep, empty_store()).unwrap();
    let o = export(&old_db, &keep, &["--force"]);
    assert!(!o.status.success(), "{o:?}");
    assert_eq!(std::fs::read(&keep).unwrap(), empty_store());
    assert_eq!(d.files(), ["keep.hxd", "old.db"]);
}

#[test]
fn export_with_a_missing_input_keeps_the_previous_export() {
    let d = Dir::new("missing");
    let prev = d.path("prev.db");
    std::fs::write(&prev, b"SQLite format 3\0 previous export").unwrap();
    let o = export(&d.path("nosuch.hxd"), &prev, &["--force"]);
    assert!(!o.status.success(), "{o:?}");
    assert_eq!(
        std::fs::read(&prev).unwrap(),
        b"SQLite format 3\0 previous export"
    );
    assert_eq!(d.files(), ["prev.db"]);
}

#[test]
fn force_does_not_replace_a_file_that_is_not_a_database() {
    let d = Dir::new("notdb");
    let (snap, notes) = (d.path("snap.hxd"), d.path("notes.txt"));
    std::fs::write(&snap, empty_store()).unwrap();
    std::fs::write(&notes, b"keep me").unwrap();
    for cmd in ["export", "db"] {
        let o = hxprobe(&[Path::new(cmd), &snap, &notes], &["--force"]);
        assert_eq!(o.status.code(), Some(1), "{cmd}: {o:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("not a SQLite database"));
        assert_eq!(std::fs::read(&notes).unwrap(), b"keep me", "{cmd}");
    }
    assert_eq!(d.files(), ["notes.txt", "snap.hxd"]);
}

#[test]
fn db_onto_its_own_input_keeps_the_input() {
    let d = Dir::new("dbsame");
    let snap = d.path("snap.hxd");
    std::fs::write(&snap, empty_store()).unwrap();
    let o = hxprobe(&[Path::new("db"), &snap, &snap], &[]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert_eq!(std::fs::read(&snap).unwrap(), empty_store());
}

#[test]
fn export_into_a_missing_directory_fails() {
    let d = Dir::new("fail");
    let snap = d.path("snap.hxd");
    std::fs::write(&snap, empty_store()).unwrap();
    // The output cannot be created: its directory does not exist.
    let o = export(&snap, &d.path("nodir").join("out.db"), &[]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert_eq!(d.files(), ["snap.hxd"]);
}

#[test]
fn dump_rejects_a_count_that_is_not_a_number() {
    let d = Dir::new("dump");
    let snap = d.path("snap.hxd");
    std::fs::write(&snap, empty_store()).unwrap();
    let o = hxprobe(
        &[Path::new("dump"), &snap, Path::new("Recipient")],
        &["abc"],
    );
    assert_eq!(o.status.code(), Some(2), "{o:?}");
    assert!(String::from_utf8_lossy(&o.stderr).contains("not a number"));
    let o = hxprobe(&[Path::new("dump"), &snap, Path::new("Recipient")], &["3"]);
    assert!(o.status.success(), "{o:?}");
}

#[test]
fn every_store_command_reports_the_checksum_budget() {
    let d = Dir::new("budget");
    let (snap, out) = (d.path("snap.hxd"), d.path("out.db"));
    std::fs::write(&snap, overlapping_blocks(64)).unwrap();
    let want = "budget hit: 59 blocks not verified";
    let runs: [(&[&Path], &[&str]); 4] = [
        (&[Path::new("blocks"), &snap], &[]),
        (&[Path::new("types"), &snap], &[]),
        (&[Path::new("dump"), &snap, Path::new("Recipient")], &["1"]),
        (&[Path::new("export"), &snap, &out], &[]),
    ];
    for (args, flags) in runs {
        let o = hxprobe(args, flags);
        assert!(o.status.success(), "{args:?}: {o:?}");
        let stdout = String::from_utf8_lossy(&o.stdout);
        assert!(stdout.contains(want), "{args:?}: {stdout}");
    }
    // A store within the budget prints no such line.
    std::fs::write(&snap, empty_store()).unwrap();
    let o = hxprobe(&[Path::new("types"), &snap], &[]);
    assert!(!String::from_utf8_lossy(&o.stdout).contains("budget hit"));
}

#[test]
fn blocks_counts_signatures_off_the_block_boundary() {
    let d = Dir::new("unaligned");
    let snap = d.path("snap.hxd");
    let mut store = empty_store();
    store.resize(1024, 0);
    let line = "signatures off a 512-byte boundary ignored";
    std::fs::write(&snap, &store).unwrap();
    let o = hxprobe(&[Path::new("blocks"), &snap], &[]);
    assert!(!String::from_utf8_lossy(&o.stdout).contains(line), "{o:?}");

    store[600..608].copy_from_slice(&hxprobe::block::MAGIC.to_le_bytes());
    std::fs::write(&snap, &store).unwrap();
    let o = hxprobe(&[Path::new("blocks"), &snap], &[]);
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(stdout.contains("0 block signatures found"), "{stdout}");
    assert!(stdout.contains(&format!("1 {line}")), "{stdout}");
}

#[cfg(unix)]
#[test]
fn a_symlink_output_is_refused_and_its_target_kept() {
    let d = Dir::new("symlink");
    let (snap, real, link) = (d.path("snap.hxd"), d.path("real.db"), d.path("link.db"));
    std::fs::write(&snap, empty_store()).unwrap();
    std::fs::write(&real, b"SQLite format 3\0 previous export").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    for (cmd, flags) in [("export", &["--force"][..]), ("export", &[]), ("db", &[])] {
        let o = hxprobe(&[Path::new(cmd), &snap, &link], flags);
        assert_eq!(o.status.code(), Some(1), "{cmd}: {o:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("symbolic link"));
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read(&real).unwrap(),
            b"SQLite format 3\0 previous export"
        );
    }
    assert_eq!(d.files(), ["link.db", "real.db", "snap.hxd"]);
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_output_is_refused() {
    let d = Dir::new("dangling");
    let (snap, link) = (d.path("snap.hxd"), d.path("link.db"));
    std::fs::write(&snap, empty_store()).unwrap();
    std::os::unix::fs::symlink(d.path("nosuch.db"), &link).unwrap();
    for cmd in ["export", "db"] {
        let o = hxprobe(&[Path::new(cmd), &snap, &link], &[]);
        assert_eq!(o.status.code(), Some(1), "{cmd}: {o:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("symbolic link"));
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }
    assert_eq!(d.files(), ["link.db", "snap.hxd"]);
}
