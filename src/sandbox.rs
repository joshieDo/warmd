use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};

use anyhow::{Context as _, ensure};
use std::io::ErrorKind;
use tokio::{
    fs::create_dir_all,
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, Command},
    time::sleep,
};
use uuid::Uuid;

use crate::{
    config::Job,
    diag::{self, Diag},
    metrics, ra,
};

const TAIL_BYTES: usize = 4000;

const BOTH_BYTES: usize = 512 * 1024;

const MOUNT_ATTEMPTS: u32 = 10;

const MOUNT_RETRY: Duration = Duration::from_millis(50);

const SLAPSLOP_CACHE: &str = "target/slapslop";

/// Where a lanes run's results directory is bound in the sandbox.
const RESULTS: &str = "/tmp/warmd-results";

/// The line a lanes run prints when a job finished, before the job's index.
const DONE_MARK: &str = "warmd-done:";

const TIMES_MARK: &str = "warmd-cpu:";

/// Where a tree sees the rustc cache: under `target/`, beside the outputs it fills.
const RUSTC_STORE: &str = "target/.warmd-rustc";

/// The directory whose [`RUSTC_STORE`] is the rustc cache: an overlay's top lower layer, so a cached output comes
/// back as a reflink (a clone stays inside one mount).
static RUSTC_LAYER: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
pub(crate) struct Sandbox<'a> {
    pub(crate) repo: &'a str,
    pub(crate) tree: Tree<'a>,
}

impl Sandbox<'_> {
    pub(crate) async fn run(self, cmd: &str, stdin: Option<&str>) -> anyhow::Result<Output> {
        self.run_described(cmd, cmd, stdin).await
    }

    pub(crate) async fn run_described(self, label: &str, script: &str, stdin: Option<&str>) -> anyhow::Result<Output> {
        metrics::note_command(label);
        self.run_quiet(script, stdin).await
    }

    async fn run_quiet(self, cmd: &str, stdin: Option<&str>) -> anyhow::Result<Output> {
        for attempt in 1..=MOUNT_ATTEMPTS {
            let out = self.run_once(cmd, stdin).await?;
            if !out.stderr.starts_with("bwrap: Can't make overlay mount") || attempt == MOUNT_ATTEMPTS {
                return Ok(out);
            }
            sleep(MOUNT_RETRY * attempt).await;
        }
        unreachable!("the last attempt returns")
    }

    async fn run_once(self, cmd: &str, stdin: Option<&str>) -> anyhow::Result<Output> {
        let mut child = self.spawn(cmd, None, stdin.is_some(), None).await?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            cpu_secs: None,
        })
    }

    /// Starts a long-running tool in the tree, its directory bound read-only and `vars` set, stdin and stdout piped.
    pub(crate) async fn serve(self, tool: &Path, vars: &[(&str, &str)]) -> anyhow::Result<Child> {
        let cmd = format!("exec '{}'", tool.display().to_string().replace('\'', "'\\''"));
        self.spawn(&cmd, None, true, Some((ra::tool_dir(tool)?, vars))).await
    }

    /// Starts bwrap running `cmd` in the tree, with `results` (a host directory) bound at [`RESULTS`] when given,
    /// stdout and stderr piped, stdin piped when `stdin`, and a tool's directory bound with its variables set.
    async fn spawn(
        self,
        cmd: &str,
        results: Option<&Path>,
        stdin: bool,
        tool: Option<(&Path, &[(&str, &str)])>,
    ) -> anyhow::Result<Child> {
        let home = env::var_os("HOME").map(PathBuf::from).context("HOME is unset")?;
        let cargo_home = env::var_os("CARGO_HOME").map_or_else(|| home.join(".cargo"), PathBuf::from);
        let rustup_home = env::var_os("RUSTUP_HOME").map_or_else(|| home.join(".rustup"), PathBuf::from);
        let cache = home.join(".cache");
        let work = work_path(self.repo);
        let mut bwrap = Command::new("bwrap");
        bwrap.args(["--die-with-parent", "--unshare-all", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
        bwrap.args(["--tmpfs", "/tmp"]);
        match self.tree {
            Tree::Build { dir, cache: repo_cache } => {
                bwrap.arg("--share-net");
                for shared in [&cargo_home, &rustup_home] {
                    bwrap.arg("--bind").arg(shared).arg(shared);
                }
                create_dir_all(&cache).await?;
                bwrap.arg("--bind").arg(repo_cache).arg(&cache);
                bwrap.arg("--bind").arg(dir).arg(&work);
                if let Some(layer) = RUSTC_LAYER.get() {
                    bwrap.arg("--bind").arg(layer.join(RUSTC_STORE)).arg(format!("{work}/{RUSTC_STORE}"));
                }
            }
            Tree::Overlay { lower, layer, cache: repo_cache } => {
                bwrap.arg("--tmpfs").arg(&home);
                bwrap.arg("--ro-bind").arg(&rustup_home).arg(&rustup_home);
                // The tools on PATH under the home the tmpfs hides (slapslop installed outside ~/.local in CI).
                let on_path = env::var_os("PATH").map_or_default(|path| env::split_paths(&path).collect::<Vec<_>>());
                for tool_dir in [home.join(".local/bin"), home.join(".local/share/slapslop")].into_iter().chain(
                    on_path.into_iter().filter(|dir| {
                        dir.starts_with(&home) && !dir.starts_with(&cargo_home) && !dir.starts_with(&rustup_home)
                    }),
                ) {
                    bwrap.arg("--ro-bind-try").arg(&tool_dir).arg(&tool_dir);
                }
                bwrap.arg("--overlay-src").arg(&cargo_home).arg("--tmp-overlay").arg(&cargo_home);
                bwrap.arg("--overlay-src").arg(repo_cache).arg("--tmp-overlay").arg(&cache);

                bwrap.args(["--setenv", "SLAPSLOP_DEPCACHE", "0"]);
                bwrap.arg("--overlay-src").arg(lower);
                if let Some(layer) = RUSTC_LAYER.get() {
                    bwrap.arg("--overlay-src").arg(layer);
                }
                bwrap.arg("--overlay").arg(layer.join("upper")).arg(layer.join("work")).arg(&work);
            }
        }
        bwrap.args(["--setenv", "SLAPSLOP_CACHE", &format!("{work}/{SLAPSLOP_CACHE}")]);
        if RUSTC_LAYER.get().is_some() {
            let wrapper = env::current_exe()?.with_file_name("warmd-rustc");
            // Bound where it is: an overlay tree hides the home it may sit under.
            bwrap.arg("--ro-bind").arg(&wrapper).arg(&wrapper).arg("--setenv").arg("RUSTC_WRAPPER").arg(&wrapper);
            bwrap.args(["--setenv", "WARMD_RUSTC_STORE", &format!("{work}/{RUSTC_STORE}")]);
        }
        if let Some(remote) = env::var_os("SLAPSLOP_REMOTE_CACHE").filter(|dir| !dir.is_empty()) {
            create_dir_all(&remote).await?;
            bwrap.arg("--bind").arg(&remote).arg(&remote).arg("--setenv").arg("SLAPSLOP_REMOTE_CACHE").arg(&remote);
        }
        if let Some(dir) = results {
            bwrap.arg("--bind").arg(dir).arg(RESULTS);
        }
        if let Some((dir, vars)) = tool {
            bwrap.arg("--ro-bind").arg(dir).arg(dir);
            for (name, value) in vars {
                bwrap.args(["--setenv", name, value]);
            }
        }
        bwrap.args(["--chdir", &work, "sh", "-c", cmd]);
        bwrap.stdin(if stdin { Stdio::piped() } else { Stdio::null() });
        bwrap.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        bwrap.spawn().context("starting bwrap")
    }

    /// Runs the lanes in one sandbox (one overlay mount: two mounts of a layer would not see each other's writes):
    /// each lane's jobs one after another, the lanes side by side. Calls `done` with each job and its output and
    /// findings as it finishes.
    pub(crate) async fn diagnose_lanes<'job>(
        self,
        lanes: &[Vec<LaneJob<'job>>],
        mut done: impl FnMut(&LaneJob<'job>, Output, Vec<Diag>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let dir = env::temp_dir().join(format!("warmd-lanes-{}", Uuid::new_v4()));
        create_dir_all(&dir).await?;
        let result = async {
            for attempt in 1..=MOUNT_ATTEMPTS {
                let mut child = self.spawn(&lanes_script(lanes), Some(&dir), false, None).await?;
                let mut lines = BufReader::new(child.stdout.take().context("bwrap stdout")?).lines();
                let mut finished = 0;
                while let Some(line) = lines.next_line().await? {
                    let Some(index) = line.strip_prefix(DONE_MARK).and_then(|index| index.parse::<usize>().ok()) else {
                        continue;
                    };
                    let job =
                        lanes.iter().flatten().find(|job| job.index == index).context("an unknown job finished")?;
                    let out = Output {
                        code: result(&dir, index, "code")?.trim().parse().ok(),
                        stdout: result(&dir, index, "out")?,
                        stderr: result(&dir, index, "err")?,
                        cpu_secs: None,
                    }
                    .with_cpu();
                    let found = if job.job.slapslop {
                        diag::parse(&result(&dir, index, "hits")?)
                    } else {
                        diag::parse(&out.stdout)
                    };
                    done(job, out, found)?;
                    finished += 1;
                }
                let status = child.wait().await?;
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    pipe.read_to_string(&mut stderr).await?;
                }
                if finished == 0 && stderr.starts_with("bwrap: Can't make overlay mount") && attempt < MOUNT_ATTEMPTS {
                    sleep(MOUNT_RETRY * attempt).await;
                    continue;
                }
                ensure!(
                    finished == lanes.iter().map(Vec::len).sum::<usize>(),
                    "the jobs' sandbox stopped early ({status}): {}",
                    stderr.trim()
                );
                return Ok(());
            }
            unreachable!("the last attempt returns")
        }
        .await;
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        result
    }

    pub(crate) async fn slapslop(self, cmd: &str) -> anyhow::Result<(Output, Vec<Diag>)> {
        let out = self
            .run_described(cmd, &timed(&format!("rm -f {SLAPSLOP_CACHE}/hits.jsonl && {cmd}")), None)
            .await?
            .with_cpu();
        Ok((
            out,
            diag::parse(
                &self.run_quiet(&format!("cat {SLAPSLOP_CACHE}/hits.jsonl 2>/dev/null || true"), None).await?.stdout,
            ),
        ))
    }
}

/// A job of a lanes run: its index in the plan, the job and the command it runs.
#[derive(Debug)]
pub(crate) struct LaneJob<'job> {
    pub(crate) index: usize,
    pub(crate) job: &'job Job,
    pub(crate) cmd: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Tree<'a> {
    Build { dir: &'a Path, cache: &'a Path },
    Overlay { lower: &'a Path, layer: &'a Path, cache: &'a Path },
}

#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    /// User plus system CPU seconds of the command and everything it ran (the shell's `times`).
    pub(crate) cpu_secs: Option<f64>,
}

impl Output {
    /// Takes the CPU line `timed` printed off stdself.
    fn with_cpu(mut self) -> Self {
        if let Some((rest, line)) = self.stdout.rsplit_once(&format!("\n{TIMES_MARK}")) {
            // The shell's times line comes first, then its children's: the job is the last one.
            self.cpu_secs =
                line.lines().last().map(|children| children.split_whitespace().filter_map(shell_secs).sum());
            self.stdout = rest.to_owned();
        }
        self
    }

    pub(crate) const fn ok(&self) -> bool {
        matches!(self.code, Some(0))
    }

    pub(crate) fn both(&self) -> String {
        format!("== stdout\n{}\n== stderr\n{}", tail_of(&self.stdout, BOTH_BYTES), tail_of(&self.stderr, BOTH_BYTES))
    }

    pub(crate) fn tail(&self) -> String {
        tail_of(if self.stderr.trim().is_empty() { &self.stdout } else { &self.stderr }, TAIL_BYTES)
    }
}

pub(crate) fn work_path(repo: &str) -> String {
    format!("/tmp/warmd/{repo}")
}

/// Turns the rustc cache on for every sandbox from now on, with `layer` holding the store, when `warmd-rustc` sits
/// next to this binary. Returns the store's `v1` directory.
pub(crate) fn use_rustc_cache(layer: PathBuf) -> anyhow::Result<Option<PathBuf>> {
    if !env::current_exe()?.with_file_name("warmd-rustc").is_file() {
        return Ok(None);
    }
    let store = layer.join(RUSTC_STORE).join("v1");
    fs::create_dir_all(&store)?;
    RUSTC_LAYER.get_or_init(|| layer);
    Ok(Some(store))
}

/// Moves what an overlay job added to the rustc cache (in its upper layer) into the store, so other trees hit it.
pub(crate) fn harvest_rustc_cache(layer: &Path) -> anyhow::Result<usize> {
    let Some(root) = RUSTC_LAYER.get() else {
        return Ok(0);
    };
    let added = layer.join("upper").join(RUSTC_STORE);
    let mut moved = 0;
    let mut pending = vec![added.join("v1")];
    while let Some(dir) = pending.pop() {
        for entry in match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
        } {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let into = root.join(RUSTC_STORE).join(path.strip_prefix(&added)?);
            if path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')) || into.exists() {
                fs::remove_file(&path)?;
            } else {
                fs::create_dir_all(into.parent().context("a store path without a directory")?)?;
                fs::rename(&path, &into)?;
                moved += 1;
            }
        }
    }
    if added.exists() {
        fs::remove_dir_all(&added).with_context(|| format!("removing {}", added.display()))?;
    }
    Ok(moved)
}

/// One file a job of a lanes run wrote to its results directory.
fn result(dir: &Path, index: usize, ext: &str) -> anyhow::Result<String> {
    let file = dir.join(format!("{index}.{ext}"));
    fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))
}

/// The script of a lanes run: a background subshell per lane running its jobs in order, each job's stdout, stderr,
/// exit status (and slapslop's hits) written to [`RESULTS`], then a [`DONE_MARK`] line with its index.
fn lanes_script(lanes: &[Vec<LaneJob<'_>>]) -> String {
    let mut script = String::new();
    for lane in lanes {
        script.push_str("(\n");
        for LaneJob { index, job, cmd } in lane {
            let at = format!("{RESULTS}/{index}");
            if job.slapslop {
                script.push_str(&format!("rm -f {SLAPSLOP_CACHE}/hits.jsonl\n"));
            }
            script.push_str(&format!("( {} ) >{at}.out 2>{at}.err\necho $? >{at}.code\n", timed(cmd)));
            if job.slapslop {
                script.push_str(&format!("cat {SLAPSLOP_CACHE}/hits.jsonl >{at}.hits 2>/dev/null || true\n"));
            }
            script.push_str(&format!("echo {DONE_MARK}{index}\n"));
        }
        script.push_str(") &\n");
    }
    script.push_str("wait\n");
    script
}

/// The command, then its children's CPU times on stdout after a marker, keeping its exit status.
fn timed(cmd: &str) -> String {
    format!(
        "{{ {cmd}
}}; warmd_status=$?; printf '\n{TIMES_MARK}'; times; exit $warmd_status"
    )
}

/// `1m2.500000s` as seconds.
fn shell_secs(text: &str) -> Option<f64> {
    let (minutes, seconds) = text.strip_suffix('s')?.split_once('m')?;
    Some(minutes.parse::<f64>().ok()?.mul_add(60.0, seconds.parse::<f64>().ok()?))
}

fn tail_of(text: &str, limit: usize) -> String {
    text.get(
        (text.len().saturating_sub(limit)..text.len())
            .find(|&index| text.is_char_boundary(index))
            .unwrap_or(text.len())..,
    )
    .unwrap_or_default()
    .to_owned()
}
