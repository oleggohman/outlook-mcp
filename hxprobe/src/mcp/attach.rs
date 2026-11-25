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
    // FIFO does not block the open.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
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
}
