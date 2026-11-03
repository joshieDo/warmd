use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::Context as _;
use tokio::{fs::create_dir_all, io::AsyncWriteExt as _, process::Command, time::sleep};

use crate::{
    config::Job,
    diag::{self, Diag},
};

const TAIL_BYTES: usize = 4000;

const MOUNT_ATTEMPTS: u32 = 10;

const MOUNT_RETRY: Duration = Duration::from_millis(50);

const SLAPSLOP_CACHE: &str = "target/slapslop";

#[derive(Debug, Clone, Copy)]
pub(crate) struct Sandbox<'a> {
    pub(crate) repo: &'a str,
    pub(crate) tree: Tree<'a>,
}

impl Sandbox<'_> {
    pub(crate) async fn run(self, cmd: &str, stdin: Option<&str>) -> anyhow::Result<Output> {
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
        let home = env::var_os("HOME").map(PathBuf::from).context("HOME is unset")?;
        let cargo_home = env::var_os("CARGO_HOME").map_or_else(|| home.join(".cargo"), PathBuf::from);
        let rustup_home = env::var_os("RUSTUP_HOME").map_or_else(|| home.join(".rustup"), PathBuf::from);
        let cache = home.join(".cache");
        create_dir_all(&cache).await?;
        let work = work_path(self.repo);
        let mut bwrap = Command::new("bwrap");
        bwrap.args(["--die-with-parent", "--unshare-all", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
        bwrap.args(["--tmpfs", "/tmp"]);
        match self.tree {
            Tree::Build(dir) => {
                bwrap.arg("--share-net");
                for shared in [&cargo_home, &rustup_home, &cache] {
                    bwrap.arg("--bind").arg(shared).arg(shared);
                }
                bwrap.arg("--bind").arg(dir).arg(&work);
            }
            Tree::Overlay { lower, upper, work: overlay_work } => {
                for scratch in [&cargo_home, &cache] {
                    bwrap.arg("--overlay-src").arg(scratch).arg("--tmp-overlay").arg(scratch);
                }
                bwrap.arg("--overlay-src").arg(lower).arg("--overlay").arg(upper).arg(overlay_work).arg(&work);
            }
        }
        bwrap.args(["--setenv", "SLAPSLOP_CACHE", &format!("{work}/{SLAPSLOP_CACHE}")]);
        bwrap.args(["--chdir", &work, "sh", "-c", cmd]);
        bwrap.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        bwrap.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = bwrap.spawn().context("starting bwrap")?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    pub(crate) async fn diagnose(self, job: &Job) -> anyhow::Result<(Output, Vec<Diag>)> {
        self.diagnose_cmd(job, &job.cmd).await
    }

    pub(crate) async fn diagnose_cmd(self, job: &Job, cmd: &str) -> anyhow::Result<(Output, Vec<Diag>)> {
        if !job.slapslop {
            let out = self.run(cmd, None).await?;
            let found = diag::parse(&out.stdout);
            return Ok((out, found));
        }
        self.slapslop(cmd).await
    }

    pub(crate) async fn slapslop(self, cmd: &str) -> anyhow::Result<(Output, Vec<Diag>)> {
        let out = self.run(&format!("rm -f {SLAPSLOP_CACHE}/hits.jsonl && {cmd}"), None).await?;
        let hits = self.run(&format!("cat {SLAPSLOP_CACHE}/hits.jsonl 2>/dev/null || true"), None).await?;
        Ok((out, diag::parse(&hits.stdout)))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Tree<'a> {
    Build(&'a Path),
    Overlay { lower: &'a Path, upper: &'a Path, work: &'a Path },
}

#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Output {
    pub(crate) const fn ok(&self) -> bool {
        matches!(self.code, Some(0))
    }

    pub(crate) fn tail(&self) -> String {
        let text = if self.stderr.trim().is_empty() { &self.stdout } else { &self.stderr };
        let start = text.len().saturating_sub(TAIL_BYTES);
        let start = (start..text.len()).find(|&index| text.is_char_boundary(index)).unwrap_or(text.len());
        text.get(start..).unwrap_or_default().to_owned()
    }
}

pub(crate) fn work_path(repo: &str) -> String {
    format!("/tmp/warmd/{repo}")
}
