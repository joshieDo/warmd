// RUSTC_WRAPPER of warmd's sandboxes: caches what rustc writes, keyed by everything it reads, in a store warmd
// shares across main builds, branches and profiles, and puts cached outputs back as reflinks (no second copy).
//
// Only plain rustc compiles of one file are cached: clippy's and slapslop's drivers, -C incremental builds and
// anything else run as they are. The key covers rustc's version, the arguments, the libraries --extern names and
// the native ones under -L (by content), the CARGO* environment, and every file and environment variable rustc's
// dep-info names (a --emit=dep-info pass runs first). A failure of the cache itself falls back to running rustc.
#![forbid(unsafe_code)]

use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fmt::Write as _,
    fs::{self, File},
    io::{self, ErrorKind, Read as _, Write as _},
    mem,
    os::unix::{fs::PermissionsExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::{self, Command, ExitCode, Stdio},
    thread,
    time::SystemTime,
};

use anyhow::{Context as _, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{from_slice, to_vec};
use sha2::{Digest as _, Sha256};

// Changing it starts a fresh key space.
const VERSION: &str = "warmd-rustc 1";

const CACHEABLE: [&str; 4] = ["lib", "rlib", "proc-macro", "bin"];

// Flags whose value is the next argument.
const WITH_VALUE: [&str; 20] = [
    "--crate-name",
    "--crate-type",
    "--edition",
    "--out-dir",
    "-C",
    "-L",
    "--cfg",
    "--check-cfg",
    "--cap-lints",
    "--extern",
    "--emit",
    "--error-format",
    "--json",
    "--target",
    "-A",
    "-W",
    "-D",
    "-F",
    "-Z",
    "--diagnostic-width",
];

// One cacheable rustc invocation.
struct Unit {
    rustc: OsString,
    args: Vec<String>,
    store: PathBuf,
    crate_name: String,
    extra: String,
    out_dir: PathBuf,
}

impl Unit {
    fn of(rustc: &OsString, args: &[OsString], store: PathBuf) -> Option<Self> {
        if Path::new(rustc).file_name()? != "rustc" {
            return None;
        }
        let args = args.iter().map(|arg| arg.to_str().map(str::to_owned)).collect::<Option<Vec<_>>>()?;
        let (mut crate_name, mut extra, mut out_dir, mut types, mut inputs) =
            (None, String::new(), None, Vec::new(), 0);
        let mut at = 0;
        while let Some(arg) = args.get(at) {
            let (flag, value) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
                _ if WITH_VALUE.contains(&arg.as_str()) => {
                    at += 1;
                    (arg.as_str(), Some(args.get(at)?.clone()))
                }
                _ => match arg.strip_prefix("-C").filter(|rest| !rest.is_empty()) {
                    Some(rest) => ("-C", Some(rest.to_owned())),
                    None => (arg.as_str(), None),
                },
            };
            match (flag, value) {
                ("--crate-name", Some(name)) => crate_name = Some(name),
                ("--crate-type", Some(kinds)) => types.extend(kinds.split(',').map(str::to_owned)),
                ("--out-dir", Some(dir)) => out_dir = Some(PathBuf::from(dir)),
                ("-C", Some(codegen)) if codegen.starts_with("incremental") => return None,
                ("-C", Some(codegen)) => {
                    if let Some(value) = codegen.strip_prefix("extra-filename=") {
                        value.clone_into(&mut extra);
                    }
                }
                ("-o" | "--print" | "-" | "--test", _) => return None,
                (input, None)
                    if !input.starts_with('-') && Path::new(input).extension().is_some_and(|ext| ext == "rs") =>
                {
                    inputs += 1;
                }
                _ => {}
            }
            at += 1;
        }
        if types.is_empty()
            || !types.iter().all(|kind| CACHEABLE.contains(&kind.as_str()))
            || inputs != 1
            || extra.is_empty()
        {
            return None;
        }
        Some(Self { rustc: rustc.clone(), args, store, crate_name: crate_name?, extra, out_dir: out_dir? })
    }

    fn run(self) -> ExitCode {
        let key = match self.key() {
            Ok(Some(key)) => key,
            Ok(None) => return exec(&self.rustc, &self.os_args()),
            Err(err) => {
                eprintln!("warmd-rustc: not cached: {err:#}");
                return exec(&self.rustc, &self.os_args());
            }
        };
        let entry_path = self.store.join("v1/units").join(key.get(..2).unwrap_or("00")).join(format!("{key}.json"));
        match self.restore(&entry_path) {
            Ok(()) => return ExitCode::SUCCESS,
            Err(err) if err.downcast_ref::<io::Error>().is_some_and(|io| io.kind() == ErrorKind::NotFound) => {}
            Err(err) => eprintln!("warmd-rustc: compiling again, the cached outputs did not come back: {err:#}"),
        }
        let started = SystemTime::now();
        let ran = match self.compile() {
            Ok(ran) => ran,
            Err(err) => {
                eprintln!("warmd-rustc: running rustc: {err:#}");
                return ExitCode::from(127);
            }
        };
        if ran.code != 0 {
            return match u8::try_from(ran.code) {
                Ok(code) => ExitCode::from(code),
                Err(err) => {
                    eprintln!("warmd-rustc: rustc exited with {}: {err}", ran.code);
                    ExitCode::FAILURE
                }
            };
        }
        if let Err(err) = self.save(&entry_path, started, ran) {
            eprintln!("warmd-rustc: not saved to the cache: {err:#}");
        }
        ExitCode::SUCCESS
    }

    fn os_args(&self) -> Vec<OsString> {
        self.args.iter().map(OsString::from).collect()
    }

    // The key, or none when rustc's dep-info pass fails (the real compile then reports why).
    fn key(&self) -> anyhow::Result<Option<String>> {
        let mut hash = Sha256::new();
        let mut field = |label: &str, bytes: &[u8]| {
            hash.update(label.as_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        };
        field("version", VERSION.as_bytes());
        let version = Command::new(&self.rustc).arg("-vV").stderr(Stdio::null()).output()?;
        ensure!(version.status.success(), "rustc -vV failed");
        field("rustc", &version.stdout);
        let mut previous = "";
        for arg in &self.args {
            match (previous, arg.split_once('=')) {
                ("--extern", Some((name, path))) => {
                    field("extern", name.as_bytes());
                    field("extern-content", &file_digest(Path::new(path))?);
                }
                ("-L", Some((kind, dir))) if kind != "dependency" && kind != "crate" => {
                    field("native", arg.as_bytes());
                    field("native-content", &dir_digest(Path::new(dir))?);
                }
                _ => field("arg", arg.as_bytes()),
            }
            previous = arg;
        }
        let mut vars = env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
            .filter(|(name, _)| (name.starts_with("CARGO") && name != "CARGO_MAKEFLAGS") || name == "RUSTC_BOOTSTRAP")
            .collect::<Vec<_>>();
        vars.sort();
        for (name, value) in vars {
            field("env", name.as_bytes());
            field("env-value", value.as_encoded_bytes());
        }
        let Some((files, env_deps)) = self.dep_info()? else {
            return Ok(None);
        };
        for file in files {
            field("file", file.as_bytes());
            field("file-content", &file_digest(Path::new(&file))?);
        }
        for dep in env_deps {
            field("env-dep", dep.as_bytes());
        }
        Ok(Some(hex(&hash.finalize())))
    }

    // The crate's files and NAME=value environment, from a --emit=dep-info pass into a scratch directory.
    fn dep_info(&self) -> anyhow::Result<Option<(BTreeSet<String>, BTreeSet<String>)>> {
        let scratch = env::temp_dir().join(format!("warmd-rustc-{}-{}", process::id(), self.extra));
        fs::create_dir_all(&scratch)?;
        let mut args = Vec::with_capacity(self.args.len());
        let mut skip = false;
        for arg in &self.args {
            if skip {
                skip = false;
            } else if arg == "--emit" || arg == "--out-dir" {
                skip = true;
            } else if !arg.starts_with("--emit=") && !arg.starts_with("--out-dir=") {
                args.push(arg.as_str());
            }
        }
        let status = Command::new(&self.rustc)
            .args(&args)
            .arg("--emit=dep-info")
            .arg("--out-dir")
            .arg(&scratch)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        let text = fs::read_to_string(scratch.join(format!("{}{}.d", self.crate_name, self.extra)));
        fs::remove_dir_all(&scratch)?;
        if !status.success() {
            return Ok(None);
        }
        let (mut files, mut vars) = (BTreeSet::new(), BTreeSet::new());
        for line in text?.lines() {
            if let Some(var) = line.strip_prefix("# env-dep:") {
                vars.insert(var.to_owned());
            } else if let Some((_, deps)) = line.split_once(": ") {
                files.extend(split_escaped(deps));
            }
        }
        Ok(Some((files, vars)))
    }

    fn restore(&self, entry_path: &Path) -> anyhow::Result<()> {
        let entry: Entry = from_slice(&fs::read(entry_path)?)?;
        for output in &entry.outputs {
            place(&self.store.join("v1/blobs").join(&output.blob), &self.out_dir.join(&output.name))?;
        }
        io::stdout().write_all(entry.stdout.as_bytes())?;
        io::stderr().write_all(entry.stderr.as_bytes())?;
        Ok(())
    }

    // Runs rustc, passing its output through as it comes and keeping a copy.
    fn compile(&self) -> anyhow::Result<Ran> {
        let mut child =
            Command::new(&self.rustc).args(&self.args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let out = child.stdout.take().context("rustc stdout")?;
        let err = child.stderr.take().context("rustc stderr")?;
        let stdout = thread::spawn(move || tee(out, io::stdout()));
        let stderr = tee(err, io::stderr())?;
        let stdout = stdout.join().map_err(|panic| anyhow!("copying rustc's stdout panicked: {panic:?}"))??;
        Ok(Ran { code: child.wait()?.code().unwrap_or(1), stdout, stderr })
    }

    fn save(&self, entry_path: &Path, started: SystemTime, ran: Ran) -> anyhow::Result<()> {
        let stem = format!("{}{}", self.crate_name, self.extra);
        let mut outputs = Vec::new();
        for item in fs::read_dir(&self.out_dir)? {
            let item = item?;
            let name =
                item.file_name().into_string().map_err(|name| anyhow!("{} is not UTF-8", name.to_string_lossy()))?;
            let rest = name.strip_prefix("lib").unwrap_or(&name).strip_prefix(&stem);
            let meta = item.metadata()?;
            if !rest.is_some_and(|rest| rest.is_empty() || rest.starts_with('.')) || !meta.is_file() {
                continue;
            }
            if meta.modified()? < started {
                continue;
            }
            let blob = hex(&file_digest(&item.path())?);
            let blob_path = self.store.join("v1/blobs").join(&blob);
            if !blob_path.exists() {
                place(&item.path(), &blob_path)?;
            }
            outputs.push(Output { name, blob, exe: meta.permissions().mode() & 0o111 != 0 });
        }
        ensure!(!outputs.is_empty(), "rustc wrote nothing named {stem} in {}", self.out_dir.display());
        write_atomic(entry_path, &to_vec(&Entry { outputs, stdout: ran.stdout, stderr: ran.stderr })?)
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    outputs: Vec<Output>,
    stdout: String,
    stderr: String,
}

#[derive(Serialize, Deserialize)]
struct Output {
    name: String,
    blob: String,
    exe: bool,
}

struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

fn main() -> ExitCode {
    let mut argv = env::args_os().skip(1);
    let Some(rustc) = argv.next() else {
        eprintln!("usage: warmd-rustc RUSTC ARGS...");
        return ExitCode::from(2);
    };
    let args = argv.collect::<Vec<_>>();
    match env::var_os("WARMD_RUSTC_STORE")
        .filter(|store| !store.is_empty())
        .and_then(|store| Unit::of(&rustc, &args, PathBuf::from(store)))
    {
        Some(unit) => unit.run(),
        None => exec(&rustc, &args),
    }
}

// Replaces this process with rustc; returns only when it could not start.
fn exec(rustc: &OsString, args: &[OsString]) -> ExitCode {
    let err = Command::new(rustc).args(args).exec();
    eprintln!("warmd-rustc: starting {}: {err}", rustc.to_string_lossy());
    ExitCode::from(127)
}

// Copies through a temporary name with copy_file_range: a reflink where the filesystem has them.
fn place(from: &Path, to: &Path) -> anyhow::Result<()> {
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

fn tee(mut from: impl io::Read, mut to: impl io::Write) -> anyhow::Result<String> {
    let mut kept = Vec::new();
    let mut buf = [0; 8192];
    loop {
        let read = from.read(&mut buf)?;
        if read == 0 {
            return Ok(String::from_utf8(kept)?);
        }
        let chunk = buf.get(..read).context("short read")?;
        to.write_all(chunk)?;
        to.flush()?;
        kept.extend_from_slice(chunk);
    }
}

fn file_digest(path: &Path) -> anyhow::Result<Vec<u8>> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            return Ok(hash.finalize().to_vec());
        }
        hash.update(buf.get(..read).context("short read")?);
    }
}

// Every regular file under dir, by relative path and content.
fn dir_digest(dir: &Path) -> anyhow::Result<Vec<u8>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(at) = pending.pop() {
        if !at.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&at)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    let mut hash = Sha256::new();
    for file in files {
        hash.update(file.strip_prefix(dir)?.as_os_str().as_encoded_bytes());
        hash.update(file_digest(&file)?);
    }
    Ok(hash.finalize().to_vec())
}

// The paths of a make rule's prerequisites, where a backslash escapes a space.
fn split_escaped(deps: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut chars = deps.chars();
    while let Some(char) = chars.next() {
        match char {
            '\\' => current.extend(chars.next()),
            ' ' => {
                if !current.is_empty() {
                    paths.push(mem::take(&mut current));
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}
