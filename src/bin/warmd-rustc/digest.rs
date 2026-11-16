//! SHA-256 digests of files, remembered in an extended attribute on the file itself (`user.warmd.d`), so describing
//! a crate whose dependencies are hundreds of megabytes is a `stat` and a `getxattr` per file rather than reading
//! them all again for every rustc invocation.
//!
//! The attribute holds the digest with the size and modification time the file had and the moment it was written.
//! It is believed while the size and modification time are the same and the file's change time, which no archive,
//! copy or `touch` can set back, is no later than [`SETTLED`] after that moment: a copy that kept the attribute, a
//! file written in place with its old time put back, a second name given to it all have it read again. A digest
//! computed by reading is remembered only once the file was last modified [`SETTLED`] ago, so a write within the
//! same tick of the filesystem's clock cannot leave a stale digest behind. 57 bytes stay inside an ext4 inode of 256
//! bytes, so remembering costs no disk block.
//!
//! Where attributes cannot be read or written (another filesystem, a read-only mount) files are read every time.
//! On overlayfs nothing is remembered on a file this process did not write: setting an attribute on a file of a
//! lower layer copies the whole file up. Files owned by another user (the system's libraries under a `-L native`
//! directory) are known by their inode, size and times instead of their content.

use std::{
    collections::HashSet,
    fs::{self, File, Metadata},
    io::Read as _,
    os::unix::fs::MetadataExt as _,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use rustix::{
    fs::{XattrFlags, getxattr, setxattr, statfs},
    process::geteuid,
};
use sha2::{Digest as _, Sha256};

/// A SHA-256 digest.
pub(crate) type Digest = [u8; 32];

const ATTR: &str = "user.warmd.d";

/// The attribute's layout: this byte, the digest, then the size, modification time and moment of writing as
/// little-endian 64-bit numbers (times in nanoseconds since the epoch).
const FORM: u8 = 1;

const ATTR_LEN: usize = 1 + 32 + 8 * 3;

/// How long a file must have been left alone before a digest read from it is remembered, and how long after
/// remembering its change time may still move: longer than the coarsest clock a filesystem stamps files with.
const SETTLED: Duration = Duration::from_secs(2);

const OVERLAYFS_MAGIC: i128 = 0x794c_7630;

/// The digest of a directory that a dependency's path only passes through.
pub(crate) const DIR_MARK: Digest = [0xd1; 32];

/// The file's digest: the remembered one while it holds, else read (and remembered when the file has settled).
pub(crate) fn file(path: &Path) -> anyhow::Result<Digest> {
    let meta = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.uid() != geteuid().as_raw() {
        return Ok(foreign(&meta));
    }
    if let Some(digest) = recorded(path, &meta) {
        return Ok(digest);
    }
    let digest = read(path)?;
    if !hot(&meta) && !on_overlay(path) {
        seal(path, &meta, &digest);
    }
    Ok(digest)
}

/// Remembers the digest of a file this process just wrote, linked or copied, so nothing reads it again.
pub(crate) fn remember(path: &Path, digest: &Digest) {
    if let Ok(meta) = fs::metadata(path) {
        seal(path, &meta, digest);
    }
}

/// Every file under `dir` (following links, each directory once), by relative path and digest; nothing when it is
/// no directory.
pub(crate) fn dir(dir: &Path) -> anyhow::Result<Digest> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_owned()];
    let mut seen = HashSet::new();
    while let Some(at) = pending.pop() {
        if !fs::metadata(&at).is_ok_and(|meta| meta.is_dir() && seen.insert((meta.dev(), meta.ino()))) {
            continue;
        }
        for entry in fs::read_dir(&at).with_context(|| format!("listing {}", at.display()))? {
            let path = entry?.path();
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => pending.push(path),
                Ok(meta) if meta.is_file() => files.push(path),
                _ => {}
            }
        }
    }
    files.sort();
    let mut hash = Sha256::new();
    for path in files {
        let name = path.strip_prefix(dir)?.as_os_str().as_encoded_bytes();
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        hash.update(file(&path)?);
    }
    Ok(hash.finalize().into())
}

/// The digest of bytes in memory.
pub(crate) fn bytes(bytes: &[u8]) -> Digest {
    Sha256::digest(bytes).into()
}

/// A digest as 64 lowercase hex characters.
pub(crate) fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A key over labelled fields, each length-prefixed so no two sequences of fields collide.
pub(crate) struct Key(Sha256);

impl Key {
    pub(crate) fn new(domain: &str) -> Self {
        let mut key = Self(Sha256::new());
        key.field("domain", domain.as_bytes());
        key
    }

    pub(crate) fn field(&mut self, label: &str, bytes: &[u8]) {
        for part in [label.as_bytes(), bytes] {
            self.0.update((part.len() as u64).to_le_bytes());
            self.0.update(part);
        }
    }

    pub(crate) fn finish(self) -> String {
        hex(&self.0.finalize())
    }
}

fn read(path: &Path) -> anyhow::Result<Digest> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buf).with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            return Ok(hash.finalize().into());
        }
        hash.update(buf.get(..read).context("short read")?);
    }
}

/// A file of another user's, by what says it changed.
fn foreign(meta: &Metadata) -> Digest {
    let mut hash = Sha256::new();
    hash.update(b"foreign");
    for number in [meta.ino(), meta.len()] {
        hash.update(number.to_le_bytes());
    }
    for number in [meta.mtime(), meta.mtime_nsec(), meta.ctime(), meta.ctime_nsec()] {
        hash.update(number.to_le_bytes());
    }
    hash.finalize().into()
}

/// The remembered digest, when the attribute was written for the file as it is.
fn recorded(path: &Path, meta: &Metadata) -> Option<Digest> {
    let mut raw = [0; ATTR_LEN];
    if getxattr(path, ATTR, &mut raw[..]).ok()? != ATTR_LEN {
        return None;
    }
    let (&form, rest) = raw.split_first()?;
    let (digest, rest) = rest.split_first_chunk::<32>()?;
    let (size, rest) = rest.split_first_chunk::<8>()?;
    let (modified, rest) = rest.split_first_chunk::<8>()?;
    let written = i64::from_le_bytes(*rest.first_chunk::<8>()?);
    let settled = i64::try_from(SETTLED.as_nanos()).ok()?;
    let holds = form == FORM
        && u64::from_le_bytes(*size) == meta.len()
        && i64::from_le_bytes(*modified) == nanos(meta.mtime(), meta.mtime_nsec())?
        && nanos(meta.ctime(), meta.ctime_nsec())? <= written.checked_add(settled)?;
    holds.then_some(*digest)
}

fn seal(path: &Path, meta: &Metadata, digest: &Digest) {
    if let Some(now) =
        SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|now| i64::try_from(now.as_nanos()).ok())
    {
        seal_at(path, meta, digest, now);
    }
}

fn seal_at(path: &Path, meta: &Metadata, digest: &Digest, written: i64) {
    let Some(modified) = nanos(meta.mtime(), meta.mtime_nsec()) else {
        return;
    };
    let mut raw = Vec::with_capacity(ATTR_LEN);
    raw.push(FORM);
    raw.extend_from_slice(digest);
    raw.extend_from_slice(&meta.len().to_le_bytes());
    raw.extend_from_slice(&modified.to_le_bytes());
    raw.extend_from_slice(&written.to_le_bytes());
    let _unsupported_or_read_only = setxattr(path, ATTR, &raw, XattrFlags::empty());
}

/// True when the file was modified so recently (or by a clock ahead of this one) that another write could leave it
/// with the same stamp.
fn hot(meta: &Metadata) -> bool {
    meta.modified().ok().and_then(|at| SystemTime::now().duration_since(at).ok()).is_none_or(|age| age < SETTLED)
}

/// Whether the path is on overlayfs.
pub(crate) fn on_overlay(path: &Path) -> bool {
    statfs(path).is_ok_and(|fs| i128::from(fs.f_type) == OVERLAYFS_MAGIC)
}

const fn nanos(secs: i64, nanos: i64) -> Option<i64> {
    match secs.checked_mul(1_000_000_000) {
        Some(at) => at.checked_add(nanos),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::FileTimes, path::PathBuf, process};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("warmd-rustc-digest-{name}-{}", process::id()));
        let _fresh = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes the file and dates it long enough ago that its digest may be remembered.
    fn settled(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        let past = SystemTime::now() - Duration::from_secs(60);
        File::options().write(true).open(path).unwrap().set_times(FileTimes::new().set_modified(past)).unwrap();
    }

    fn keeps_attributes(path: &Path) -> bool {
        setxattr(path, "user.warmd.probe", b"1", XattrFlags::empty()).is_ok()
    }

    #[test]
    fn a_remembered_digest_is_read_from_the_file_until_the_file_changes() {
        let dir = scratch("remembered");
        let path = dir.join("lib.rlib");
        settled(&path, b"first");
        if !keeps_attributes(&path) {
            return;
        }
        assert_eq!(file(&path).unwrap(), bytes(b"first"));
        let mut raw = [0; ATTR_LEN];
        assert_eq!(getxattr(&path, ATTR, &mut raw[..]).unwrap(), ATTR_LEN, "the digest was remembered");

        let marked = [7; 32];
        remember(&path, &marked);
        assert_eq!(file(&path).unwrap(), marked, "the remembered digest is believed");

        settled(&path, b"other");
        assert_eq!(file(&path).unwrap(), bytes(b"other"), "same size, but a new time");
        fs::write(&path, b"longer content").unwrap();
        assert_eq!(file(&path).unwrap(), bytes(b"longer content"));
    }

    #[test]
    fn a_digest_is_not_believed_once_the_file_changed_after_it_was_written() {
        let dir = scratch("changed");
        let path = dir.join("lib.rlib");
        settled(&path, b"bytes");
        if !keeps_attributes(&path) {
            return;
        }
        let meta = fs::metadata(&path).unwrap();
        let long_ago = nanos(meta.ctime(), meta.ctime_nsec()).unwrap() - 10_000_000_000;
        seal_at(&path, &fs::metadata(&path).unwrap(), &[7; 32], long_ago);
        assert_eq!(file(&path).unwrap(), bytes(b"bytes"), "an attribute copied or kept over a rewrite is not believed");
    }

    #[test]
    fn a_file_written_a_moment_ago_is_read_but_not_remembered() {
        let dir = scratch("hot");
        let path = dir.join("lib.rmeta");
        fs::write(&path, b"fresh").unwrap();
        assert_eq!(file(&path).unwrap(), bytes(b"fresh"));
        let mut raw = [0; ATTR_LEN];
        assert!(getxattr(&path, ATTR, &mut raw[..]).is_err());
    }

    #[test]
    fn a_directory_digest_follows_every_file_under_it() {
        let dir = scratch("dir");
        fs::create_dir_all(dir.join("sub")).unwrap();
        settled(&dir.join("libz.a"), b"z");
        settled(&dir.join("sub/libz.a"), b"z");
        let before = super::dir(&dir).unwrap();
        assert_eq!(super::dir(&dir).unwrap(), before);
        settled(&dir.join("sub/libz.a"), b"y");
        assert_ne!(super::dir(&dir).unwrap(), before, "a changed file");
        let changed = super::dir(&dir).unwrap();
        settled(&dir.join("libnew.a"), b"");
        assert_ne!(super::dir(&dir).unwrap(), changed, "an added file");
        assert_eq!(super::dir(&dir.join("missing")).unwrap(), super::dir(&scratch("empty")).unwrap());
        std::os::unix::fs::symlink(&dir, dir.join("sub/loop")).unwrap();
        assert!(super::dir(&dir).is_ok(), "a link back up is walked once");
    }
}
