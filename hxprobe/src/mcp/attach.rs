//! Attachment files of the Outlook profile: the checked path of a stored
//! file name, the read-only open and the read.
//!
//! An `AttachmentHeader` names its file in `Filename` as
//! `~/Files/<dirs>/<name>`, where `~/` stands for the profile directory (the
//! directory that holds `HxStore.hxd`). That value comes from the store, so
//! from data mail can influence; it has to pass every check of [`open`]
//! before a file is opened. The path of a file never leaves this module.

use std::fs::{File, Metadata};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Largest file the server reads at all, in bytes.
pub const MAX_SOURCE_BYTES: u64 = 30_000_000;
/// Default of `max_bytes`, the largest file sent as bytes (provisional; the
/// hand check of the clients decides).
pub const DEFAULT_SEND_BYTES: i64 = 250_000;
/// Largest value of `max_bytes`.
pub const MAX_SEND_BYTES: i64 = 10_000_000;
/// Largest image sent, whatever `max_bytes` says.
pub const MAX_IMAGE_BYTES: i64 = 3_750_000;
/// Largest `offset` into the text of a file, in characters.
pub const MAX_TEXT_OFFSET: i64 = 30_000_000;
/// Bytes from the start of a file that decide its kind.
pub const HEAD_BYTES: usize = 8_192;

/// The cause given to `Cache::denied_here` when the attachment folder, a
/// file in it, or the store's directory cannot be read.
pub const ATTACHMENTS_DENIED: &str =
    "the attachment files of the Outlook profile cannot be read: permission denied";

/// The prefix every usable stored file name starts with.
const PREFIX: &str = "~/Files/";
/// Longest stored file name, in bytes.
const MAX_NAME: usize = 1024;
/// Longest component of a stored file name, in bytes.
const MAX_COMPONENT: usize = 255;

/// Why a file is not opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// No file (or no attachment folder) where the name points.
    NotOnThisMac,
    /// The name points outside the attachment folder.
    Outside,
    /// Not a regular file: a symbolic link, a directory, a device, a FIFO or
    /// a socket (also the attachment folder itself when it is not a real
    /// directory).
    NotRegular,
    /// Permission denied on the folder, a directory below it or the file.
    Denied,
    /// Another failure of the file system.
    Io(ErrorKind),
}

/// The checked path of `filename` under `profile` (steps 1 to 5), with the
/// device and inode the check saw. `profile` must be canonical.
fn checked(profile: &Path, filename: &str) -> Result<(PathBuf, u64, u64), Refused> {
    // Steps 1 and 2: the string alone.
    if !filename.starts_with(PREFIX) || filename.len() > MAX_NAME || filename.contains('\0') {
        return Err(Refused::Outside);
    }
    let rest = &filename[2..];
    let bad = |c: &str| c.is_empty() || c.len() > MAX_COMPONENT || c == "." || c == "..";
    if rest.split('/').any(bad) {
        return Err(Refused::Outside);
    }
    // Step 3: the attachment folder is a real directory.
    let folder = profile.join("Files");
    let m = std::fs::symlink_metadata(&folder).map_err(|e| refused(&e))?;
    if !m.file_type().is_dir() {
        return Err(Refused::NotRegular);
    }
    // Step 4: the candidate itself is a regular file.
    let candidate = rest
        .split('/')
        .fold(profile.to_path_buf(), |p, c| p.join(c));
    let m = std::fs::symlink_metadata(&candidate).map_err(|e| refused(&e))?;
    if !regular(&m) {
        return Err(Refused::NotRegular);
    }
    // Step 5: no directory on the way leads out of the folder.
    let real = std::fs::canonicalize(&candidate).map_err(|e| refused(&e))?;
    if !real.starts_with(&folder) || real == folder {
        return Err(Refused::Outside);
    }
    // The identity of the path that is opened.
    let m = std::fs::symlink_metadata(&real).map_err(|e| refused(&e))?;
    Ok((real, m.dev(), m.ino()))
}

/// Whether `m` is of a regular file (not a link, directory, device, FIFO or
/// socket).
fn regular(m: &Metadata) -> bool {
    // On Unix `is_file` holds for regular files only.
    m.file_type().is_file()
}

/// The checked path of `filename` under `profile` (steps 1 to 5). Private
/// to the read; [`open`] uses [`checked`], which also gives the identity.
#[cfg(test)]
fn resolve(profile: &Path, filename: &str) -> Result<PathBuf, Refused> {
    checked(profile, filename).map(|(p, ..)| p)
}

/// The [`Refused`] of a failed look at or open of a path.
fn refused(e: &std::io::Error) -> Refused {
    // A last component that is a symbolic link, under O_NOFOLLOW, or a loop
    // of links; std has no ErrorKind of its own for it.
    if e.raw_os_error() == Some(libc::ELOOP) {
        return Refused::NotRegular;
    }
    match e.kind() {
        ErrorKind::NotFound | ErrorKind::NotADirectory => Refused::NotOnThisMac,
        ErrorKind::PermissionDenied => Refused::Denied,
        kind => Refused::Io(kind),
    }
}

/// [`resolve`], then the read-only open (step 6): the open file and its
/// length. The path is not returned.
pub fn open(profile: &Path, filename: &str) -> Result<(File, u64), Refused> {
    let (path, dev, ino) = checked(profile, filename)?;
    // Step 6: read-only; a link put in place meanwhile fails with ELOOP, a
    // FIFO does not block the open, a terminal does not become ours.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(&path)
        .map_err(|e| refused(&e))?;
    let m = file.metadata().map_err(|e| refused(&e))?;
    if !regular(&m) || m.dev() != dev || m.ino() != ino {
        return Err(Refused::NotRegular);
    }
    Ok((file, m.len()))
}

/// At most `max` bytes from the start of `file`.
pub fn read_head(file: &mut File, max: usize) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let len = file.metadata()?.len();
    let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(max).min(max));
    file.take(max as u64).read_to_end(&mut out)?;
    #[cfg(test)]
    BYTES_READ.with(|c| c.set(c.get() + out.len() as u64));
    Ok(out)
}

/// The part of a stored file name after the last `/`.
pub fn own_name(filename: &str) -> &str {
    filename.rsplit('/').next().unwrap_or(filename)
}

/// What a file is, as far as what is sent depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Html,
    /// An image both clients show, with its media type.
    Image(&'static str),
    Pdf,
    /// A ZIP archive; it may be a Word, Excel or PowerPoint file.
    Zip,
    File,
}

/// The kind of a file from its first bytes (`head`, at most
/// [`HEAD_BYTES`] are looked at), the extension of the stored name and the
/// stored content type. A signature always wins; the extension and the
/// content type only select text, and only when no NUL byte is in the head
/// or the head starts with a UTF-16 byte order mark.
pub fn classify(head: &[u8], extension: Option<&str>, content_type: Option<&str>) -> Kind {
    if let Some(mime) = image_type(head) {
        return Kind::Image(mime);
    }
    if head.starts_with(b"%PDF-") {
        return Kind::Pdf;
    }
    if head.starts_with(b"PK\x03\x04") {
        return Kind::Zip;
    }
    // UTF-16 text has a NUL in every second byte when it is mostly ASCII;
    // with a byte order mark the NUL test is skipped.
    let utf16 = head.starts_with(b"\xff\xfe") || head.starts_with(b"\xfe\xff");
    if !utf16 && head[..head.len().min(HEAD_BYTES)].contains(&0) {
        return Kind::File;
    }
    let ext = extension.map(str::to_ascii_lowercase);
    let ext = ext.as_deref();
    // The media type without its parameters.
    let ct = content_type.map(|c| {
        c.split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    });
    let ct = ct.as_deref();
    if matches!(ext, Some("html" | "htm")) || ct == Some("text/html") {
        return Kind::Html;
    }
    let text_ext = matches!(
        ext,
        Some(
            "txt"
                | "csv"
                | "tsv"
                | "log"
                | "md"
                | "json"
                | "xml"
                | "ics"
                | "vcf"
                | "eml"
                | "yaml"
                | "yml"
        )
    );
    let text_ct = ct.is_some_and(|c| {
        c.starts_with("text/")
            || matches!(c, "application/json" | "application/xml" | "message/rfc822")
    });
    if text_ext || text_ct {
        Kind::Text
    } else {
        Kind::File
    }
}

/// The media type of an image both clients show, from its first bytes.
fn image_type(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if head.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Media type of a file without a signature, by extension.
pub fn media_type(extension: Option<&str>) -> &'static str {
    let ext = extension.map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("doc") => "application/msword",
        Some("xls") => "application/vnd.ms-excel",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("rtf") => "application/rtf",
        Some("msg") => "application/vnd.ms-outlook",
        Some("zip") => "application/zip",
        Some("7z") => "application/x-7z-compressed",
        Some("gz") => "application/gzip",
        Some("tif" | "tiff") => "image/tiff",
        Some("bmp") => "image/bmp",
        Some("heic") => "image/heic",
        Some("svg") => "image/svg+xml",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// Text of a text file: by byte order mark (UTF-8, UTF-16LE, UTF-16BE),
/// else UTF-8; invalid bytes are replaced. The flag says whether any were.
pub fn decode_text(bytes: &[u8]) -> (String, bool) {
    if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        return utf8(rest);
    }
    if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        return utf16(rest, u16::from_be_bytes);
    }
    utf8(bytes)
}

/// `bytes` as UTF-8, invalid bytes replaced; whether any were.
fn utf8(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

/// `bytes` as UTF-16 units made by `unit`; a lone surrogate and an odd last
/// byte are replaced. Whether anything was.
fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> (String, bool) {
    let units = bytes.as_chunks::<2>().0.iter().map(|c| unit(*c));
    let mut replaced = false;
    let mut out: String = char::decode_utf16(units)
        .map(|r| {
            r.unwrap_or_else(|_| {
                replaced = true;
                char::REPLACEMENT_CHARACTER
            })
        })
        .collect();
    if bytes.len() % 2 == 1 {
        out.push(char::REPLACEMENT_CHARACTER);
        replaced = true;
    }
    (out, replaced)
}

/// `text` without control characters other than `\n` and `\t` (so `\r\n`
/// becomes `\n`), as [`crate::mcp::text::cap`] drops them.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| !c.is_control() || c == '\n' || c == '\t')
        .collect()
}

/// One page of the cleaned text of a file. All counts are in characters of
/// the cleaned text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub text: String,
    pub total_chars: usize,
    /// Whether characters after this page exist.
    pub truncated: bool,
    /// Where the next page starts, when there is one.
    pub next_offset: Option<usize>,
}

/// `max_chars` characters of the cleaned `text` after `offset`; counting,
/// skipping and cutting all on `clean(text)`.
pub fn page(text: &str, offset: usize, max_chars: usize) -> Page {
    let cleaned = clean(text);
    let total_chars = cleaned.chars().count();
    let text: String = cleaned.chars().skip(offset).take(max_chars).collect();
    let end = offset.saturating_add(text.chars().count());
    let truncated = end < total_chars;
    Page {
        text,
        total_chars,
        truncated,
        next_offset: truncated.then_some(end),
    }
}

/// Standard base64 (RFC 4648) with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
thread_local! {
    /// Bytes [`read_head`] has read on this thread.
    static BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Set the byte counter of this thread to 0.
#[cfg(test)]
pub fn bytes_read_reset() {
    BYTES_READ.with(|c| c.set(0));
}

/// Bytes [`read_head`] has read on this thread since the last reset.
#[cfg(test)]
pub fn bytes_read() -> u64 {
    BYTES_READ.with(|c| c.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const ATT: &str = "Files/S0/3/Attachments/0";

    /// A fresh directory under the system temp dir that plays the profile
    /// directory (canonical, with `Files/S0/3/Attachments/0/` inside),
    /// removed on drop; every mode a test changed is restored first.
    struct Profile {
        root: PathBuf,
        locked: Vec<PathBuf>,
    }

    impl Profile {
        fn new(name: &str) -> Profile {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "hxprobe-attach-test-{}-{}-{name}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(p.join(ATT)).unwrap();
            Profile {
                root: std::fs::canonicalize(&p).unwrap(),
                locked: Vec::new(),
            }
        }

        /// A directory beside the profile, outside it.
        fn outside(&self) -> PathBuf {
            let p = self.root.join("outside");
            std::fs::create_dir_all(&p).unwrap();
            p
        }

        /// Write `bytes` to the attachment file `name`; its path.
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.root.join(ATT).join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        }

        /// Set the mode of `p`; restored to 0700 on drop.
        fn lock(&mut self, p: &Path, mode: u32) {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
            self.locked.push(p.to_path_buf());
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            for p in &self.locked {
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Whether the tests run as root, for whom mode 000 does not deny.
    fn is_root() -> bool {
        // SAFETY: geteuid() has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    fn stored(name: &str) -> String {
        format!("~/{ATT}/{name}")
    }

    #[test]
    fn good_names_resolve() {
        let p = Profile::new("good");
        for name in [
            "plain[1].txt",
            "with space[2].pdf",
            "a..b[3].pdf",
            "שלום עולם[4].txt",
            "報告 (final)[5].docx",
        ] {
            let want = p.file(name, b"x");
            assert_eq!(resolve(&p.root, &stored(name)), Ok(want), "{name}");
        }
    }

    #[test]
    fn names_outside_the_folder_are_refused() {
        let p = Profile::new("outside");
        std::fs::write(p.root.join("HxStore.hxd"), b"store").unwrap();
        p.file("x", b"x");
        let long_name = format!("~/Files/{}x", "a/".repeat(510));
        assert!(long_name.len() > MAX_NAME);
        let long_component = format!("~/Files/{}", "a".repeat(256));
        let nul = format!("~/{ATT}/a\0b");
        let names = [
            "",
            "report.pdf",
            "/etc/hosts",
            "~/Other/x",
            "~/Files",
            "~/Files/",
            "~Files/x",
            "~/files/x",
            "~/Files/../HxStore.hxd",
            "~/Files/S0/../../HxStore.hxd",
            "~/Files/S0/3/Attachments/0/..",
            "~/Files/./S0/3/Attachments/0/x",
            "~/Files/S0/3/Attachments/0/./x",
            "~/Files/.",
            "~/Files//S0/3/Attachments/0/x",
            "~/Files/S0/3/Attachments/0/x/",
            "~/./Files/S0/3/Attachments/0/x",
            "~//Files/S0/3/Attachments/0/x",
            &long_name,
            &long_component,
            &nul,
        ];
        for name in names {
            assert_eq!(resolve(&p.root, name), Err(Refused::Outside), "{name:?}");
            assert_eq!(
                open(&p.root, name).err(),
                Some(Refused::Outside),
                "{name:?}"
            );
        }
        // A component of exactly 255 bytes is allowed.
        let ok = "b".repeat(255);
        let want = p.file(&ok, b"x");
        assert_eq!(resolve(&p.root, &stored(&ok)), Ok(want));
    }

    #[test]
    fn files_that_is_a_link_is_refused() {
        let p = Profile::new("files-link");
        let out = p.outside();
        std::fs::create_dir_all(out.join("S0/3/Attachments/0")).unwrap();
        std::fs::write(out.join("S0/3/Attachments/0/a.txt"), b"x").unwrap();
        std::fs::remove_dir_all(p.root.join("Files")).unwrap();
        std::os::unix::fs::symlink(&out, p.root.join("Files")).unwrap();
        assert_eq!(resolve(&p.root, &stored("a.txt")), Err(Refused::NotRegular));
        assert_eq!(
            open(&p.root, &stored("a.txt")).err(),
            Some(Refused::NotRegular)
        );
    }

    #[test]
    fn files_that_is_a_plain_file_is_refused() {
        let p = Profile::new("files-file");
        std::fs::remove_dir_all(p.root.join("Files")).unwrap();
        std::fs::write(p.root.join("Files"), b"x").unwrap();
        assert_eq!(resolve(&p.root, &stored("a.txt")), Err(Refused::NotRegular));
    }

    #[test]
    fn a_file_that_is_a_link_is_refused() {
        let p = Profile::new("file-link");
        let inside = p.file("real.txt", b"x");
        let out = p.outside().join("secret.txt");
        std::fs::write(&out, b"x").unwrap();
        let dir = p.root.join(ATT);
        std::os::unix::fs::symlink(&inside, dir.join("in.txt")).unwrap();
        std::os::unix::fs::symlink(&out, dir.join("out.txt")).unwrap();
        for name in ["in.txt", "out.txt"] {
            assert_eq!(resolve(&p.root, &stored(name)), Err(Refused::NotRegular));
            assert_eq!(
                open(&p.root, &stored(name)).err(),
                Some(Refused::NotRegular)
            );
        }
    }

    #[test]
    fn a_directory_and_a_fifo_are_refused() {
        let p = Profile::new("special");
        std::fs::create_dir(p.root.join(ATT).join("dir")).unwrap();
        assert_eq!(resolve(&p.root, &stored("dir")), Err(Refused::NotRegular));
        assert_eq!(
            open(&p.root, &stored("dir")).err(),
            Some(Refused::NotRegular)
        );

        let fifo = p.root.join(ATT).join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert_eq!(resolve(&p.root, &stored("pipe")), Err(Refused::NotRegular));
        assert_eq!(
            open(&p.root, &stored("pipe")).err(),
            Some(Refused::NotRegular)
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_directory_link_out_of_the_folder_is_refused() {
        let p = Profile::new("dir-link-out");
        let out = p.outside();
        std::fs::write(out.join("x.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(&out, p.root.join(ATT).join("sub")).unwrap();
        assert_eq!(
            resolve(&p.root, &stored("sub/x.txt")),
            Err(Refused::Outside)
        );
        assert_eq!(
            open(&p.root, &stored("sub/x.txt")).err(),
            Some(Refused::Outside)
        );
    }

    #[test]
    fn a_directory_link_inside_the_folder_resolves() {
        let p = Profile::new("dir-link-in");
        let target = p.root.join("Files/S0");
        std::fs::write(target.join("y.txt"), b"yy").unwrap();
        std::os::unix::fs::symlink(&target, p.root.join(ATT).join("in")).unwrap();
        assert_eq!(
            resolve(&p.root, &stored("in/y.txt")),
            Ok(target.join("y.txt"))
        );
        let (_, len) = open(&p.root, &stored("in/y.txt")).unwrap();
        assert_eq!(len, 2);
    }

    #[test]
    fn open_errors_are_mapped() {
        let e = |code| refused(&std::io::Error::from_raw_os_error(code));
        assert_eq!(e(libc::ELOOP), Refused::NotRegular);
        assert_eq!(e(libc::ENOENT), Refused::NotOnThisMac);
        assert_eq!(e(libc::ENOTDIR), Refused::NotOnThisMac);
        assert_eq!(e(libc::EACCES), Refused::Denied);
        assert_eq!(e(libc::EPERM), Refused::Denied);
        assert!(matches!(e(libc::EIO), Refused::Io(_)));
    }

    #[test]
    fn missing_files_are_not_on_this_mac() {
        let p = Profile::new("missing");
        for name in [stored("none.txt"), "~/Files/S0/9/none.txt".to_string()] {
            assert_eq!(resolve(&p.root, &name), Err(Refused::NotOnThisMac));
            assert_eq!(open(&p.root, &name).err(), Some(Refused::NotOnThisMac));
        }
        // A plain file where a directory is expected.
        p.file("plain", b"x");
        let name = stored("plain/x.txt");
        assert_eq!(open(&p.root, &name).err(), Some(Refused::NotOnThisMac));
        // No attachment folder at all.
        std::fs::remove_dir_all(p.root.join("Files")).unwrap();
        assert_eq!(
            open(&p.root, &stored("a.txt")).err(),
            Some(Refused::NotOnThisMac)
        );
    }

    #[test]
    fn denied_folder_and_file_are_denied() {
        if is_root() {
            return;
        }
        let mut p = Profile::new("denied");
        let f = p.file("a.txt", b"x");
        p.lock(&f, 0o000);
        assert_eq!(open(&p.root, &stored("a.txt")).err(), Some(Refused::Denied));
        let files = p.root.join("Files");
        p.lock(&files, 0o000);
        assert_eq!(resolve(&p.root, &stored("a.txt")), Err(Refused::Denied));
        assert_eq!(open(&p.root, &stored("a.txt")).err(), Some(Refused::Denied));
    }

    /// Names, lengths, times and modes of everything under `dir`, sorted.
    fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, i64, i64, u32)> {
        let mut out = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                let m = std::fs::symlink_metadata(&p).unwrap();
                if m.is_dir() {
                    todo.push(p.clone());
                }
                out.push((p, m.len(), m.mtime(), m.mtime_nsec(), m.mode()));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn open_and_read_head_read_only() {
        let p = Profile::new("read");
        let body: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let f = p.file("data[1].bin", &body);
        let before = snapshot(&p.root);

        let (mut file, len) = open(&p.root, &stored("data[1].bin")).unwrap();
        assert_eq!(len, body.len() as u64);
        bytes_read_reset();
        let head = read_head(&mut file, 100).unwrap();
        assert_eq!(head, &body[..100]);
        assert_eq!(bytes_read(), 100);
        // Always from the start, whatever was read before.
        let all = read_head(&mut file, len as usize).unwrap();
        assert_eq!(all, body);
        assert_eq!(bytes_read(), 100 + body.len() as u64);
        // More than the file holds gives the file.
        bytes_read_reset();
        assert_eq!(read_head(&mut file, 1 << 20).unwrap(), body);
        assert_eq!(bytes_read(), body.len() as u64);
        assert_eq!(read_head(&mut file, 0).unwrap(), b"");
        drop(file);

        assert_eq!(std::fs::read(&f).unwrap(), body);
        assert_eq!(snapshot(&p.root), before);
    }

    #[test]
    fn own_name_is_the_last_component() {
        assert_eq!(
            own_name("~/Files/S0/3/Attachments/0/report[7].pdf"),
            "report[7].pdf"
        );
        assert_eq!(own_name("report.pdf"), "report.pdf");
        assert_eq!(own_name("a/b"), "b");
        assert_eq!(own_name("~/Files/S0/"), "");
        assert_eq!(own_name(""), "");
    }

    // --- kinds ---

    #[test]
    fn signatures_decide_the_kind() {
        let images: [(&[u8], &str); 5] = [
            (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", "image/png"),
            (b"\xff\xd8\xff\xe0\0\x10JFIF", "image/jpeg"),
            (b"GIF87a\x01\0\x01\0", "image/gif"),
            (b"GIF89a\x01\0\x01\0", "image/gif"),
            (b"RIFF\x24\0\0\0WEBPVP8 ", "image/webp"),
        ];
        for (head, mime) in images {
            for (ext, ct) in [
                (Some("txt"), Some("text/plain")),
                (Some("pdf"), Some("application/pdf")),
                (None, None),
            ] {
                assert_eq!(classify(head, ext, ct), Kind::Image(mime), "{mime} {ext:?}");
            }
        }
        assert_eq!(
            classify(b"%PDF-1.7\n", Some("txt"), Some("text/plain")),
            Kind::Pdf
        );
        assert_eq!(classify(b"%PDF-", None, None), Kind::Pdf);
        assert_eq!(
            classify(b"PK\x03\x04\x14\0", Some("txt"), Some("text/plain")),
            Kind::Zip
        );
        // Not quite a signature.
        assert_eq!(
            classify(b"RIFF\x24\0\0\0WAVEfmt ", Some("wav"), None),
            Kind::File
        );
        assert_eq!(classify(b"%PDF", Some("bin"), None), Kind::File);
    }

    #[test]
    fn text_kinds_come_from_the_extension_or_the_content_type() {
        for ext in [
            "txt", "csv", "tsv", "log", "md", "json", "xml", "ics", "vcf", "eml", "yaml", "yml",
            "TXT", "Csv",
        ] {
            assert_eq!(
                classify(b"a,b\n1,2\n", Some(ext), None),
                Kind::Text,
                "{ext}"
            );
        }
        for ct in [
            "text/plain",
            "text/csv; charset=utf-8",
            "Text/Calendar",
            "application/json",
            "application/xml;charset=UTF-8",
            "message/rfc822",
            " APPLICATION/JSON ",
        ] {
            assert_eq!(classify(b"hello", None, Some(ct)), Kind::Text, "{ct}");
        }
        for (ext, ct) in [
            (Some("html"), None),
            (Some("HTM"), None),
            (None, Some("text/html; charset=utf-8")),
            (Some("txt"), Some("TEXT/HTML")),
            (Some("html"), Some("text/plain")),
        ] {
            assert_eq!(classify(b"<p>x</p>", ext, ct), Kind::Html, "{ext:?} {ct:?}");
        }
        // An empty file with a text extension.
        assert_eq!(classify(b"", Some("txt"), None), Kind::Text);
        // Text bytes under an image name are no image.
        assert_eq!(
            classify(b"just text", Some("png"), Some("image/png")),
            Kind::File
        );
        assert_eq!(
            classify(b"just text", Some("bin"), Some("application/octet-stream")),
            Kind::File
        );
        assert_eq!(classify(b"just text", None, None), Kind::File);
    }

    #[test]
    fn a_nul_byte_in_the_head_makes_a_file() {
        let mut head = vec![b'a'; HEAD_BYTES];
        head[HEAD_BYTES - 1] = 0;
        assert_eq!(classify(&head, Some("txt"), Some("text/plain")), Kind::File);
        assert_eq!(classify(b"<p>\0</p>", Some("html"), None), Kind::File);
        assert_eq!(classify(b"\0", None, Some("text/plain")), Kind::File);
        // A NUL after the head is not looked at.
        let mut long = vec![b'a'; HEAD_BYTES + 1];
        long[HEAD_BYTES] = 0;
        assert_eq!(classify(&long, Some("txt"), None), Kind::Text);
    }

    #[test]
    fn utf16_text_with_a_byte_order_mark_is_text() {
        let s = "name,amount\nAnna,12\n";
        let mut le = b"\xff\xfe".to_vec();
        le.extend(utf16le(s));
        let mut be = b"\xfe\xff".to_vec();
        be.extend(utf16be(s));
        for bytes in [&le, &be] {
            assert_eq!(classify(bytes, Some("csv"), None), Kind::Text);
            assert_eq!(classify(bytes, Some("txt"), None), Kind::Text);
            assert_eq!(classify(bytes, None, Some("text/plain")), Kind::Text);
            assert_eq!(classify(bytes, Some("htm"), None), Kind::Html);
            assert_eq!(decode_text(bytes), (s.to_string(), false));
            // The extension or the content type still has to say text.
            assert_eq!(classify(bytes, Some("bin"), None), Kind::File);
        }
        // Without a byte order mark a NUL still makes a file.
        assert_eq!(classify(&utf16le(s), Some("csv"), None), Kind::File);
    }

    #[test]
    fn media_types_by_extension() {
        let table = [
            ("doc", "application/msword"),
            ("xls", "application/vnd.ms-excel"),
            ("ppt", "application/vnd.ms-powerpoint"),
            ("rtf", "application/rtf"),
            ("msg", "application/vnd.ms-outlook"),
            ("zip", "application/zip"),
            ("7z", "application/x-7z-compressed"),
            ("gz", "application/gzip"),
            ("tif", "image/tiff"),
            ("tiff", "image/tiff"),
            ("bmp", "image/bmp"),
            ("heic", "image/heic"),
            ("svg", "image/svg+xml"),
            ("mp3", "audio/mpeg"),
            ("wav", "audio/wav"),
            ("mp4", "video/mp4"),
            ("mov", "video/quicktime"),
        ];
        for (ext, mime) in table {
            assert_eq!(media_type(Some(ext)), mime, "{ext}");
            assert_eq!(media_type(Some(&ext.to_uppercase())), mime, "{ext}");
        }
        assert_eq!(media_type(Some("exe")), "application/octet-stream");
        assert_eq!(media_type(Some("")), "application/octet-stream");
        assert_eq!(media_type(None), "application/octet-stream");
    }

    // --- text ---

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn utf16be(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_be_bytes).collect()
    }

    #[test]
    fn text_is_decoded_by_its_byte_order_mark() {
        let s = "Hello, שלום עולם 😀";
        assert_eq!(decode_text(s.as_bytes()), (s.to_string(), false));
        let mut b = b"\xef\xbb\xbf".to_vec();
        b.extend_from_slice(s.as_bytes());
        assert_eq!(decode_text(&b), (s.to_string(), false));

        let mut le = b"\xff\xfe".to_vec();
        le.extend(utf16le(s));
        assert_eq!(decode_text(&le), (s.to_string(), false));
        let mut be = b"\xfe\xff".to_vec();
        be.extend(utf16be(s));
        assert_eq!(decode_text(&be), (s.to_string(), false));

        // An odd last byte is replaced.
        le.push(b'x');
        assert_eq!(decode_text(&le), (format!("{s}\u{fffd}"), true));
        be.push(b'x');
        assert_eq!(decode_text(&be), (format!("{s}\u{fffd}"), true));
        // A lone surrogate is replaced.
        let mut lone = b"\xff\xfe".to_vec();
        lone.extend_from_slice(&[0x00, 0xd8, b'a', 0]);
        assert_eq!(decode_text(&lone), ("\u{fffd}a".to_string(), true));

        assert_eq!(decode_text(b""), (String::new(), false));
        assert_eq!(decode_text(b"\xef\xbb\xbf"), (String::new(), false));
        assert_eq!(decode_text(b"\xff\xfe"), (String::new(), false));
    }

    #[test]
    fn invalid_utf8_is_replaced_and_flagged() {
        assert_eq!(
            decode_text(b"caf\xe9 ok"),
            ("caf\u{fffd} ok".to_string(), true)
        );
        assert_eq!(
            decode_text(b"\xef\xbb\xbfa\xffb"),
            ("a\u{fffd}b".to_string(), true)
        );
    }

    #[test]
    fn clean_drops_control_characters_but_lines_and_tabs() {
        assert_eq!(clean("a\r\nb"), "a\nb");
        assert_eq!(clean("a\tb\nc"), "a\tb\nc");
        assert_eq!(
            clean("a\u{0}b\u{7}c\u{1b}[0md\u{7f}e\u{85}f\rg"),
            "abc[0mdefg"
        );
        assert_eq!(clean("שלום\r\n"), "שלום\n");
        assert_eq!(clean(""), "");
    }

    #[test]
    fn pages_of_a_text() {
        let text = "abcdefghij";
        let p = page(text, 0, 4);
        assert_eq!(
            p,
            Page {
                text: "abcd".into(),
                total_chars: 10,
                truncated: true,
                next_offset: Some(4)
            }
        );
        let p = page(text, 4, 4);
        assert_eq!(
            (p.text.as_str(), p.truncated, p.next_offset),
            ("efgh", true, Some(8))
        );
        let p = page(text, 8, 4);
        assert_eq!(
            (p.text.as_str(), p.truncated, p.next_offset),
            ("ij", false, None)
        );
        // Exactly to the end.
        let p = page(text, 6, 4);
        assert_eq!(
            (p.text.as_str(), p.truncated, p.next_offset),
            ("ghij", false, None)
        );
        for offset in [10, 11, usize::MAX] {
            let p = page(text, offset, 4);
            assert_eq!(
                p,
                Page {
                    text: String::new(),
                    total_chars: 10,
                    truncated: false,
                    next_offset: None
                }
            );
        }
        // Characters, not bytes; counted on the cleaned text.
        let p = page("שלום\r\nעולם", 0, 5);
        assert_eq!(p.total_chars, 9);
        assert_eq!(p.text, "שלום\n");
        assert_eq!(p.next_offset, Some(5));
        assert_eq!(page("שלום\r\nעולם", 5, 100).text, "עולם");
        assert_eq!(page("", 0, 5).total_chars, 0);
    }

    /// The pages of `text` read one after the other with `max_chars`.
    fn read_all(text: &str, max_chars: usize) -> (String, usize, usize) {
        let mut out = String::new();
        let mut offset = 0;
        let mut pages = 0;
        loop {
            let p = page(text, offset, max_chars);
            assert!(p.text.chars().count() <= max_chars);
            out.push_str(&p.text);
            pages += 1;
            match p.next_offset {
                Some(n) => {
                    assert!(p.truncated);
                    assert_eq!(n, offset + p.text.chars().count());
                    offset = n;
                }
                None => {
                    assert!(!p.truncated);
                    return (out, p.total_chars, pages);
                }
            }
        }
    }

    #[test]
    fn reading_page_by_page_gives_the_whole_cleaned_text() {
        let plain: String = (0..50).map(|i| format!("line {i} שלום\n")).collect();
        let (all, total, pages) = read_all(&plain, 7);
        assert_eq!(all, plain);
        assert_eq!(total, plain.chars().count());
        assert_eq!(pages, total.div_ceil(7));

        let crlf: String = (0..50).map(|i| format!("row {i}\r\n")).collect();
        let want = crlf.replace("\r\n", "\n");
        let (all, total, _) = read_all(&crlf, 7);
        assert_eq!(all, want);
        assert_eq!(total, want.chars().count());
        // Every page text is what cap leaves unchanged.
        let mut offset = 0;
        while let Some(n) = page(&crlf, offset, 7).next_offset {
            let p = page(&crlf, offset, 7);
            assert_eq!(crate::mcp::text::cap(&p.text, 7).0, p.text);
            offset = n;
        }
    }

    // --- base64 ---

    #[test]
    fn base64_of_rfc_4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input}");
        }
    }

    #[test]
    fn base64_of_every_byte_value() {
        let all: Vec<u8> = (0..=255u8).collect();
        let want = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4\
                    OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3Bx\
                    cnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmq\
                    q6ytrq+wsbKztLW2t7i5uru8vb6/wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj\
                    5OXm5+jp6uvs7e7v8PHy8/T19vf4+fr7/P3+/w==";
        assert_eq!(base64(&all), want);
        assert_eq!(base64(&all).len(), 344);
    }
}
