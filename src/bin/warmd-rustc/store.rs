//! The store under `WARMD_RUSTC_STORE`/v2:
//!
//! - `pre/<ab>/<pre-key>/<manifest>.json`: the source files (with their digests) and the variables rustc's dep-info
//!   named, for each compile under that pre-key. A manifest that matches the files and environment as they are now
//!   gives the action key, with no rustc started.
//! - `units/<ab>/<action>.json`: the outputs (by blob) and messages of the compile under that action key.
//! - `blobs/<digest>`: output files. A `.rlib` with an `.rmeta` beside it enters and leaves the store as a hard link
//!   where the filesystem allows (rustc replaces its outputs by rename, never writing into them), so a restore costs
//!   no copy; everything else is copied (a reflink where the filesystem has them): cargo dates a crate by its newest
//!   output, which the fresh `.rmeta` then is, and linkers write executables and shared objects in place.
//! - `toolchains/<id>`: `rustc -vV` per toolchain binary, so it is not started for every invocation.

use std::{
    collections::{BTreeMap, HashSet},
    env,
    ffi::OsStr,
    fs,
    io::{self, Write as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{from_slice, to_vec};

use crate::digest::{self, DIR_MARK, Key};

/// The layout's directory: changing it starts an empty store.
pub(crate) const LAYOUT: &str = "v2";

/// How many of a pre-key's manifests a lookup tries, newest first.
const MANIFESTS: usize = 64;

/// How far behind the precise clock a filesystem's timestamps may lag.
const CLOCK_LAG: Duration = Duration::from_millis(20);

/// One compile that rustc finished, to be stored.
pub(crate) struct Compiled<'a> {
    pub(crate) dep_info: &'a Path,
    pub(crate) out_dir: &'a Path,
    pub(crate) stem: &'a str,
    pub(crate) started: SystemTime,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Entry {
    outputs: Vec<Output>,
    stdout: String,
    stderr: String,
}

#[derive(Serialize, Deserialize)]
struct Output {
    name: String,
    blob: String,
    link: bool,
}

/// What a compile read beyond its pre-key: files by path and digest, in rustc's order, and the variables `env!` and
/// `option_env!` looked up (none for one that was unset).
#[derive(Serialize, Deserialize)]
struct Manifest {
    files: Vec<(String, String)>,
    env: BTreeMap<String, Option<String>>,
}

pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn new(dir: &Path) -> Self {
        Self { root: dir.join(LAYOUT) }
    }

    /// What `rustc -vV` says, remembered for the toolchain binary rustc stands for.
    pub(crate) fn toolchain(&self, rustc: &OsStr) -> anyhow::Result<String> {
        let path =
            toolchain_id(rustc).map(|id| self.root.join("toolchains").join(digest::hex(&digest::bytes(id.as_bytes()))));
        if let Some(path) = &path
            && let Ok(text) = fs::read_to_string(path)
        {
            return Ok(text);
        }
        let out = Command::new(rustc).arg("-vV").stderr(Stdio::null()).output()?;
        ensure!(out.status.success(), "rustc -vV failed");
        let text = String::from_utf8(out.stdout)?;
        if let Some(path) = &path {
            write_atomic(path, text.as_bytes())?;
        }
        Ok(text)
    }

    /// The entry of the newest manifest under `pre` that the files and environment match now.
    pub(crate) fn lookup(&self, pre: &str) -> anyhow::Result<Option<Entry>> {
        let dir = self.manifests(pre);
        for name in newest_first(&dir)?.into_iter().take(MANIFESTS) {
            let manifest: Manifest = from_slice(&fs::read(dir.join(format!("{name}.json")))?)?;
            if !manifest.holds() {
                continue;
            }
            match fs::read(self.unit(&action(pre, &name))) {
                Ok(bytes) => return Ok(Some(from_slice(&bytes)?)),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    let _raced_or_read_only = fs::remove_file(dir.join(format!("{name}.json")));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(None)
    }

    /// Puts the entry's outputs into `out_dir`, then replays rustc's messages (cargo starts dependents on them).
    /// Nothing is linked on overlayfs, where a link to a file of a lower layer copies the file up first.
    pub(crate) fn restore(&self, entry: &Entry, out_dir: &Path) -> anyhow::Result<()> {
        let may_link = !digest::on_overlay(out_dir);
        for output in &entry.outputs {
            let blob = self.blob(&output.blob);
            let digest = from_hex(&output.blob).context("a blob named by no digest")?;
            let meta = fs::metadata(&blob).with_context(|| format!("reading {}", blob.display()))?;
            if meta.nlink() > 1 && digest::file(&blob)? != digest {
                let _gone_already = fs::remove_file(&blob);
                bail!("{} no longer holds what was stored", blob.display());
            }
            let to = out_dir.join(&output.name);
            if output.link && may_link {
                link_or_copy(&blob, &to)?;
            } else {
                copy(&blob, &to)?;
            }
            digest::remember(&to, &digest);
        }
        io::stdout().write_all(entry.stdout.as_bytes())?;
        io::stderr().write_all(entry.stderr.as_bytes())?;
        Ok(())
    }

    /// Stores what rustc wrote under the key of `pre` and the manifest its dep-info gives.
    pub(crate) fn save(&self, pre: &str, compiled: Compiled<'_>) -> anyhow::Result<()> {
        let manifest = Manifest::read(compiled.dep_info, compiled.started)?;
        let manifest_bytes = to_vec(&manifest)?;
        let manifest_name = digest::hex(&digest::bytes(&manifest_bytes));
        let mut written = Vec::new();
        for item in fs::read_dir(compiled.out_dir)? {
            let item = item?;
            let name =
                item.file_name().into_string().map_err(|name| anyhow!("{} is not UTF-8", name.to_string_lossy()))?;
            let rest = name.strip_prefix("lib").unwrap_or(&name).strip_prefix(compiled.stem);
            let meta = item.metadata()?;
            if rest.is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
                && meta.is_file()
                && meta.modified()? >= compiled.started
            {
                written.push((name, item.path()));
            }
        }
        ensure!(!written.is_empty(), "rustc wrote nothing named {} in {}", compiled.stem, compiled.out_dir.display());
        let beside_rmeta = written.iter().any(|(_, path)| has_extension(path, "rmeta"));
        let mut outputs = Vec::with_capacity(written.len());
        for (name, path) in written {
            let digest = digest::file(&path)?;
            let blob_name = digest::hex(&digest);
            let blob = self.blob(&blob_name);
            let link = beside_rmeta && has_extension(&path, "rlib");
            if !blob.exists() {
                if link {
                    link_or_copy(&path, &blob)?;
                } else {
                    copy(&path, &blob)?;
                }
                digest::remember(&blob, &digest);
            }
            digest::remember(&path, &digest);
            outputs.push(Output { name, blob: blob_name, link });
        }
        let entry = Entry { outputs, stdout: compiled.stdout, stderr: compiled.stderr };
        write_atomic(&self.unit(&action(pre, &manifest_name)), &to_vec(&entry)?)?;
        let manifest_path = self.manifests(pre).join(format!("{manifest_name}.json"));
        if !manifest_path.exists() {
            write_atomic(&manifest_path, &manifest_bytes)?;
            self.forget_oldest(pre)?;
        }
        Ok(())
    }

    /// Removes the manifests of `pre` past the [`MANIFESTS`] newest, which no lookup reads.
    fn forget_oldest(&self, pre: &str) -> anyhow::Result<()> {
        let dir = self.manifests(pre);
        for name in newest_first(&dir)?.into_iter().skip(MANIFESTS) {
            let _raced = fs::remove_file(dir.join(format!("{name}.json")));
        }
        Ok(())
    }

    fn manifests(&self, pre: &str) -> PathBuf {
        self.root.join("pre").join(pre.get(..2).unwrap_or("00")).join(pre)
    }

    fn unit(&self, action: &str) -> PathBuf {
        self.root.join("units").join(action.get(..2).unwrap_or("00")).join(format!("{action}.json"))
    }

    fn blob(&self, name: &str) -> PathBuf {
        self.root.join("blobs").join(name)
    }
}

impl Manifest {
    /// The files and variables a dep-info file names, refusing a file that changed after rustc started.
    fn read(dep_info: &Path, started: SystemTime) -> anyhow::Result<Self> {
        let text = fs::read_to_string(dep_info).with_context(|| format!("reading {}", dep_info.display()))?;
        let (mut files, mut seen, mut env) = (Vec::new(), HashSet::new(), BTreeMap::new());
        for line in text.lines() {
            if let Some(var) = line.strip_prefix("# env-dep:") {
                match var.split_once('=') {
                    Some((name, value)) => env.insert(name.to_owned(), Some(unescape_env(value))),
                    None => env.insert(var.to_owned(), None),
                };
            } else if let Some((_, deps)) = line.split_once(": ").filter(|_| !line.starts_with('#')) {
                for file in split_escaped(deps) {
                    if !seen.insert(file.clone()) {
                        continue;
                    }
                    let modified = fs::metadata(&file).and_then(|meta| meta.modified())?;
                    ensure!(modified + CLOCK_LAG < started, "{file} changed while rustc ran");
                    let digest = current(Path::new(&file))?;
                    files.push((file, digest::hex(&digest)));
                }
            }
        }
        Ok(Self { files, env })
    }

    fn holds(&self) -> bool {
        self.env.iter().all(|(name, value)| env::var(name).ok() == *value)
            && self
                .files
                .iter()
                .all(|(path, recorded)| current(Path::new(path)).is_ok_and(|now| digest::hex(&now) == *recorded))
    }
}

/// The names of the manifests in `dir`, newest first; none when there is no such directory.
fn newest_first(dir: &Path) -> anyhow::Result<Vec<String>> {
    let listed = match fs::read_dir(dir) {
        Ok(listed) => listed,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("listing {}", dir.display())),
    };
    let mut found = Vec::new();
    for item in listed {
        let item = item?;
        if let Some(name) = item.file_name().to_str().and_then(|name| name.strip_suffix(".json"))
            && !name.starts_with('.')
        {
            found.push((item.metadata()?.modified()?, name.to_owned()));
        }
    }
    found.sort_unstable_by(|a, b| b.cmp(a));
    Ok(found.into_iter().map(|(_, name)| name).collect())
}

/// The program a command name runs: itself when it names a path, else the first match on `PATH`.
pub(crate) fn which(program: &OsStr) -> Option<PathBuf> {
    if Path::new(program).components().count() > 1 {
        return Some(PathBuf::from(program));
    }
    env::split_paths(&env::var_os("PATH")?).map(|dir| dir.join(program)).find(|path| path.is_file())
}

/// A file by where it really is and what says it changed, for programs that are not hashed.
pub(crate) fn identity(path: &Path) -> Option<String> {
    let real = fs::canonicalize(path).ok()?;
    let meta = fs::metadata(&real).ok()?;
    Some(format!("{} {} {} {}.{}", real.display(), meta.len(), meta.ino(), meta.mtime(), meta.mtime_nsec()))
}

/// The toolchain binary a rustc command runs, through a rustup proxy and the toolchain it was told to use, with its
/// library directory; none when that cannot be told without running it.
fn toolchain_id(rustc: &OsStr) -> Option<String> {
    let found = which(rustc)?;
    let canonical = fs::canonicalize(&found).ok()?;
    let rustup = found.with_file_name("rustup");
    let proxy = canonical.file_name().is_some_and(|name| name == "rustup")
        || fs::metadata(&rustup)
            .ok()
            .zip(fs::metadata(&found).ok())
            .is_some_and(|(rustup, this)| rustup.dev() == this.dev() && rustup.ino() == this.ino());
    let real = if proxy {
        let toolchain = env::var_os("RUSTUP_TOOLCHAIN").filter(|name| !name.is_empty())?;
        let dir = if Path::new(&toolchain).components().count() > 1 {
            PathBuf::from(toolchain)
        } else {
            let home = env::var_os("RUSTUP_HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")))?;
            home.join("toolchains").join(toolchain)
        };
        dir.join("bin").join("rustc")
    } else {
        canonical
    };
    let lib = real.parent()?.parent()?.join("lib");
    Some(format!("{}\n{}", identity(&real)?, identity(&lib)?))
}

fn action(pre: &str, manifest: &str) -> String {
    let mut key = Key::new("action");
    key.field("pre", pre.as_bytes());
    key.field("manifest", manifest.as_bytes());
    key.finish()
}

/// A dependency's digest as it is now: a directory that a path only passes through by a marker.
fn current(path: &Path) -> anyhow::Result<digest::Digest> {
    if fs::metadata(path)?.is_dir() {
        return Ok(DIR_MARK);
    }
    digest::file(path)
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension().is_some_and(|found| found == extension)
}

fn from_hex(text: &str) -> Option<digest::Digest> {
    let mut digest = [0; 32];
    if text.len() != 64 {
        return None;
    }
    for (byte, pair) in digest.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(digest)
}

/// Gives `to` the file's inode where both are on one mount, else a copy.
fn link_or_copy(from: &Path, to: &Path) -> anyhow::Result<()> {
    let tmp = temp_beside(to)?;
    let _none_left = fs::remove_file(&tmp);
    if fs::hard_link(from, &tmp).is_err() {
        fs::copy(from, &tmp).with_context(|| format!("copying {} to {}", from.display(), tmp.display()))?;
    }
    fs::rename(&tmp, to)?;
    Ok(())
}

/// Copies through a temporary name with `copy_file_range`: a reflink where the filesystem has them.
fn copy(from: &Path, to: &Path) -> anyhow::Result<()> {
    let tmp = temp_beside(to)?;
    fs::copy(from, &tmp).with_context(|| format!("copying {} to {}", from.display(), tmp.display()))?;
    fs::rename(&tmp, to)?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = temp_beside(path)?;
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn temp_beside(path: &Path) -> anyhow::Result<PathBuf> {
    let dir = path.parent().context("a path without a directory")?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().context("a path without a name")?.to_string_lossy();
    Ok(dir.join(format!(".{name}.{}.tmp", process::id())))
}

/// The paths of a make rule's prerequisites, where a backslash escapes a space.
fn split_escaped(deps: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut chars = deps.chars().peekable();
    while let Some(char) = chars.next() {
        match char {
            '\\' if chars.peek() == Some(&' ') => current.extend(chars.next()),
            ' ' => {
                if !current.is_empty() {
                    paths.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(char),
        }
    }
    if !current.is_empty() {
        paths.push(current);
    }
    paths
}

/// An env-dep value as the variable holds it: rustc escapes backslashes, newlines and carriage returns.
fn unescape_env(text: &str) -> String {
    let mut value = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        if char != '\\' {
            value.push(char);
            continue;
        }
        match chars.next() {
            Some('n') => value.push('\n'),
            Some('r') => value.push('\r'),
            Some('\\') | None => value.push('\\'),
            Some(other) => value.extend(['\\', other]),
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("warmd-rustc-store-{name}-{}", process::id()));
        let _fresh = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn settled(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        let past = SystemTime::now() - Duration::from_secs(60);
        File::options().write(true).open(path).unwrap().set_times(FileTimes::new().set_modified(past)).unwrap();
    }

    /// Stores a compile of `src/lib.rs` that read the variables `env_deps`, as rustc would have written it.
    fn compile(root: &Path, store: &Store, pre: &str, env_deps: &str) {
        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        let started = SystemTime::now() - Duration::from_millis(100);
        let src = root.join("src/lib.rs");
        let dep_info = out.join("x-1.d");
        fs::write(&dep_info, format!("{d}: {s}\n\n{s}:\n{env_deps}", d = dep_info.display(), s = src.display()))
            .unwrap();
        fs::write(out.join("libx-1.rlib"), b"rlib").unwrap();
        fs::write(out.join("libx-1.rmeta"), b"rmeta").unwrap();
        fs::write(out.join("libx-10.rlib"), b"another crate").unwrap();
        let compiled = Compiled {
            dep_info: &dep_info,
            out_dir: &out,
            stem: "x-1",
            started,
            stdout: String::new(),
            stderr: String::new(),
        };
        store.save(pre, compiled).unwrap();
    }

    #[test]
    fn a_hit_needs_the_sources_and_variables_a_compile_read() {
        let root = scratch("hit");
        fs::create_dir_all(root.join("src")).unwrap();
        settled(&root.join("src/lib.rs"), b"pub fn a() {}");
        let store = Store::new(&root.join("store"));
        let pre = "ab".repeat(32);
        compile(&root, &store, &pre, "# env-dep:WARMD_TEST_NEVER_SET\n");

        let entry = store.lookup(&pre).unwrap().expect("the compile is found");
        let names = entry.outputs.iter().map(|output| output.name.as_str()).collect::<HashSet<_>>();
        assert_eq!(names, HashSet::from(["x-1.d", "libx-1.rlib", "libx-1.rmeta"]), "only this crate's outputs");
        assert!(store.lookup(&"cd".repeat(32)).unwrap().is_none(), "another pre-key");

        let restored = root.join("restored");
        fs::create_dir_all(&restored).unwrap();
        store.restore(&entry, &restored).unwrap();
        assert_eq!(fs::read(restored.join("libx-1.rlib")).unwrap(), b"rlib");
        assert_eq!(fs::read(restored.join("libx-1.rmeta")).unwrap(), b"rmeta");
        if !digest::on_overlay(&restored) {
            assert_eq!(fs::metadata(restored.join("libx-1.rlib")).unwrap().nlink(), 3, "out, store and restored");
        }
        assert_eq!(fs::metadata(restored.join("libx-1.rmeta")).unwrap().nlink(), 1, "metadata is always a copy");

        settled(&root.join("src/lib.rs"), b"pub fn b() {}");
        assert!(store.lookup(&pre).unwrap().is_none(), "a changed source");
        settled(&root.join("src/lib.rs"), b"pub fn a() {}");
        assert!(store.lookup(&pre).unwrap().is_some(), "the source as it was compiled");
    }

    #[test]
    fn a_variable_env_named_must_hold_the_value_it_had() {
        let root = scratch("env");
        fs::create_dir_all(root.join("src")).unwrap();
        settled(&root.join("src/lib.rs"), b"pub fn a() {}");
        let store = Store::new(&root.join("store"));
        let pre = "ef".repeat(32);
        compile(&root, &store, &pre, "# env-dep:WARMD_TEST_NEVER_SET=was\\nset\n");
        assert!(store.lookup(&pre).unwrap().is_none());
        let path = env::var("PATH").unwrap().replace('\\', "\\\\");
        compile(&root, &store, &pre, &format!("# env-dep:PATH={path}\n"));
        assert!(store.lookup(&pre).unwrap().is_some());
    }

    #[test]
    fn a_pre_key_keeps_only_the_manifests_a_lookup_reads() {
        let root = scratch("forget");
        fs::create_dir_all(root.join("src")).unwrap();
        let store = Store::new(&root.join("store"));
        let pre = "aa".repeat(32);
        for version in 0..=MANIFESTS {
            settled(&root.join("src/lib.rs"), format!("pub fn f{version}() {{}}").as_bytes());
            compile(&root, &store, &pre, "");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(newest_first(&store.manifests(&pre)).unwrap().len(), MANIFESTS);
        assert!(store.lookup(&pre).unwrap().is_some(), "the newest compile");
        settled(&root.join("src/lib.rs"), b"pub fn f0() {}");
        assert!(store.lookup(&pre).unwrap().is_none(), "the oldest manifest was forgotten");
    }

    #[test]
    fn a_source_written_while_rustc_ran_is_not_stored() {
        let root = scratch("racy");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub fn a() {}").unwrap();
        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        let dep_info = out.join("x.d");
        fs::write(&dep_info, format!("{}: {}\n", dep_info.display(), root.join("src/lib.rs").display())).unwrap();
        let compiled = Compiled {
            dep_info: &dep_info,
            out_dir: &out,
            stem: "x",
            started: SystemTime::now() - Duration::from_secs(1),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(Store::new(&root.join("store")).save("00", compiled).is_err());
    }

    #[test]
    fn dep_info_paths_and_variables_unescape() {
        assert_eq!(
            split_escaped("a.rs dir\\ with\\ spaces/b.rs  c\\d.rs"),
            ["a.rs", "dir with spaces/b.rs", "c\\d.rs"]
        );
        assert_eq!(unescape_env("a\\nb\\\\c\\rd"), "a\nb\\c\rd");
    }

    #[test]
    fn a_toolchain_is_told_from_the_binary_its_rustup_proxy_runs() {
        let root = scratch("toolchain");
        fs::create_dir_all(root.join("tc/bin")).unwrap();
        fs::create_dir_all(root.join("tc/lib")).unwrap();
        fs::write(root.join("tc/bin/rustc"), b"#!/bin/sh\n").unwrap();
        let id = toolchain_id(root.join("tc/bin/rustc").as_os_str()).unwrap();
        assert!(id.starts_with(&fs::canonicalize(root.join("tc/bin/rustc")).unwrap().display().to_string()));
        assert!(toolchain_id(OsStr::new("/nonexistent/rustc")).is_none());
    }
}
