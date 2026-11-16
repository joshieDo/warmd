//! `RUSTC_WRAPPER` of warmd's sandboxes: caches what rustc writes, keyed by everything it reads, in a store warmd
//! shares across main builds, branches and profiles.
//!
//! Only plain rustc compiles of one file are cached: clippy's and slapslop's drivers, an incremental compile that
//! starts from state the crate left in its incremental directory, and anything else run as they are. A lookup
//! starts no rustc (see [`store`]): a pre-key over rustc's version, the arguments, the working directory, the
//! libraries `--extern` names and the native ones under `-L` (by content) and the CARGO* environment names the
//! manifests of earlier compiles, each listing the source files and variables that compile's dep-info named, and the
//! first manifest that matches the files and environment as they are gives the entry. Digests of files are
//! remembered on the files ([`digest`]), so describing a crate costs a `stat` per file. A failure of the cache
//! itself falls back to running rustc.
//!
//! An incremental compile that starts from nothing is cached like any other, as shippy does: its result is what any
//! compile from nothing writes. A hit leaves the incremental directory as it is (the crate's next compile then
//! starts from nothing again, and is cached in turn); a compile that starts from the crate's state is left alone,
//! since what rustc writes from an earlier session is never what it writes from nothing.
#![forbid(unsafe_code)]

mod digest;
mod store;

use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, ErrorKind},
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::SystemTime,
};

use anyhow::{Context as _, anyhow};

use crate::{
    digest::Key,
    store::{Compiled, Store},
};

/// Changing it starts a fresh key space.
const VERSION: &str = "warmd-rustc 2";

const CACHEABLE: [&str; 5] = ["lib", "rlib", "proc-macro", "bin", "cdylib"];

/// Flags whose value is the next argument.
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

/// One cacheable rustc invocation.
struct Unit {
    rustc: OsString,
    args: Vec<String>,
    store: Store,
    stem: String,
    out_dir: PathBuf,
}

impl Unit {
    fn of(rustc: &OsString, args: &[OsString], store: &Path) -> Option<Self> {
        if Path::new(rustc).file_name()? != "rustc" {
            return None;
        }
        let args = args.iter().map(|arg| arg.to_str().map(str::to_owned)).collect::<Option<Vec<_>>>()?;
        let (mut crate_name, mut extra, mut out_dir, mut types, mut inputs) =
            (None, String::new(), None, Vec::new(), 0);
        let (mut emits, mut incremental) = (Vec::new(), None);
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
                ("--emit", Some(kinds)) => emits.extend(kinds.split(',').map(str::to_owned)),
                ("-C", Some(codegen)) => {
                    if let Some(value) = codegen.strip_prefix("extra-filename=") {
                        value.clone_into(&mut extra);
                    } else if let Some(dir) = codegen.strip_prefix("incremental=") {
                        incremental = Some(PathBuf::from(dir));
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
        let crate_name = crate_name?;
        if types.is_empty()
            || !types.iter().all(|kind| CACHEABLE.contains(&kind.as_str()))
            || inputs != 1
            || !emits.iter().any(|kind| kind == "dep-info")
            || emits.iter().any(|kind| kind.contains('='))
            || incremental.is_some_and(|dir| has_state(&dir, &crate_name))
        {
            return None;
        }
        Some(Self {
            rustc: rustc.clone(),
            args,
            store: Store::new(store),
            stem: format!("{crate_name}{extra}"),
            out_dir: out_dir?,
        })
    }

    fn run(self) -> ExitCode {
        let pre = match self.pre_key() {
            Ok(pre) => pre,
            Err(err) => {
                eprintln!("warmd-rustc: not cached: {err:#}");
                return exec(&self.rustc, &self.os_args());
            }
        };
        match self.store.lookup(&pre) {
            Ok(Some(entry)) => match self.store.restore(&entry, &self.out_dir) {
                Ok(()) => return ExitCode::SUCCESS,
                Err(err) => eprintln!("warmd-rustc: compiling again, the cached outputs did not come back: {err:#}"),
            },
            Ok(None) => {}
            Err(err) => eprintln!("warmd-rustc: compiling, the cache could not be read: {err:#}"),
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
        let compiled = Compiled {
            dep_info: &self.out_dir.join(format!("{}.d", self.stem)),
            out_dir: &self.out_dir,
            stem: &self.stem,
            started,
            stdout: ran.stdout,
            stderr: ran.stderr,
        };
        if let Err(err) = self.store.save(&pre, compiled) {
            eprintln!("warmd-rustc: not saved to the cache: {err:#}");
        }
        ExitCode::SUCCESS
    }

    fn os_args(&self) -> Vec<OsString> {
        self.args.iter().map(OsString::from).collect()
    }

    /// Everything the compile reads that is known before it runs.
    fn pre_key(&self) -> anyhow::Result<String> {
        let mut key = Key::new(VERSION);
        key.field("rustc", self.store.toolchain(&self.rustc)?.as_bytes());
        key.field("cwd", env::current_dir()?.as_os_str().as_encoded_bytes());
        let mut previous = "";
        for arg in &self.args {
            match (previous, arg.split_once('=')) {
                ("--extern", Some((name, path))) => {
                    key.field("extern", name.as_bytes());
                    key.field("extern-content", &digest::file(Path::new(path))?);
                }
                ("-L", Some((kind, _))) if kind == "dependency" || kind == "crate" => key.field("arg", arg.as_bytes()),
                ("-L", found) => {
                    key.field("native", arg.as_bytes());
                    key.field("native-content", &digest::dir(Path::new(found.map_or(arg.as_str(), |(_, dir)| dir)))?);
                }
                _ => key.field("arg", arg.as_bytes()),
            }
            previous = arg;
        }
        let mut vars = env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
            .filter(|(name, _)| (name.starts_with("CARGO") && name != "CARGO_MAKEFLAGS") || name == "RUSTC_BOOTSTRAP")
            .collect::<Vec<_>>();
        vars.sort();
        for (name, value) in vars {
            key.field("env", name.as_bytes());
            key.field("env-value", value.as_encoded_bytes());
        }
        Ok(key.finish())
    }

    /// Runs rustc, passing its output through as it comes and keeping a copy.
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
        .and_then(|store| Unit::of(&rustc, &args, Path::new(&store)))
    {
        Some(unit) => unit.run(),
        None => exec(&rustc, &args),
    }
}

/// Whether the incremental directory holds a session of a crate of this name: rustc keeps each unit's state in
/// `<dir>/<crate>-<id>`, the id a hash only rustc derives, so any directory of the crate's name counts (a package's
/// executable of the library's name then compiles uncached after the library compiled in the same directory).
fn has_state(dir: &Path, crate_name: &str) -> bool {
    let prefix = format!("{crate_name}-");
    match fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix)),
        Err(err) => err.kind() != ErrorKind::NotFound,
    }
}

/// Replaces this process with rustc; returns only when it could not start.
fn exec(rustc: &OsString, args: &[OsString]) -> ExitCode {
    let err = Command::new(rustc).args(args).exec();
    eprintln!("warmd-rustc: starting {}: {err}", rustc.to_string_lossy());
    ExitCode::from(127)
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

#[cfg(test)]
mod tests {
    use std::process;

    use super::*;

    fn unit(args: &str) -> Option<Unit> {
        let args = args.split(' ').map(OsString::from).collect::<Vec<_>>();
        Unit::of(&OsString::from("rustc"), &args, Path::new("/nonexistent"))
    }

    const LIB: &str =
        "--crate-name demo --edition=2024 src/lib.rs --error-format=json --emit=dep-info,metadata,link --out-dir out";

    #[test]
    fn libraries_executables_and_c_libraries_are_cached() {
        for kinds in ["lib", "rlib", "proc-macro", "bin", "lib --crate-type cdylib", "cdylib,rlib"] {
            assert!(unit(&format!("{LIB} --crate-type {kinds}")).is_some(), "{kinds}");
        }
        let cached = unit(&format!("{LIB} --crate-type lib -C extra-filename=-ab12")).unwrap();
        assert_eq!(cached.stem, "demo-ab12");
        assert!(unit(&format!("{LIB} --crate-type staticlib")).is_none());
        assert!(unit(&format!("{LIB} --crate-type lib --test")).is_none());
        let no_dep_info = "--crate-name demo src/lib.rs --emit=metadata,link --out-dir out --crate-type lib";
        assert!(unit(no_dep_info).is_none(), "the dep-info names what the compile read");
        assert!(unit(&format!("{LIB} --crate-type lib --emit=link=elsewhere.rlib")).is_none());
    }

    #[test]
    fn an_incremental_compile_is_cached_only_from_nothing() {
        let dir = env::temp_dir().join(format!("warmd-rustc-incremental-{}", process::id()));
        let _fresh = fs::remove_dir_all(&dir);
        let compile = format!("{LIB} --crate-type lib -C incremental={}", dir.display());
        assert!(unit(&compile).is_some(), "no incremental directory yet");
        fs::create_dir_all(dir.join("demo_other-1x2y")).unwrap();
        assert!(unit(&compile).is_some(), "only another crate's state");
        fs::create_dir_all(dir.join("demo-1x2y")).unwrap();
        assert!(unit(&compile).is_none(), "the crate's own state");
        assert!(unit(&format!("{LIB} --crate-type lib -Cincremental={}", dir.display())).is_none());
    }
}
