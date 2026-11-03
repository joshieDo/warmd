use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, bail, ensure};
use serde::Serialize;
use serde_json::to_vec;
use serde_json::{from_slice, json};
use tokio::{
    fs::{remove_file, rename, symlink_metadata, write},
    process::Command,
    sync::{Semaphore, SemaphorePermit},
    time::sleep,
};

use crate::{
    config::RepoCfg,
    diag::Diag,
    metrics::{self, Clock, Phase},
    sandbox::{Sandbox, Tree},
};

type Baseline = BTreeMap<String, Vec<Diag>>;

#[derive(Debug)]
pub(crate) struct Repo {
    name: String,
    cfg: RepoCfg,
    slots: Semaphore,
    jobs_dir: PathBuf,
    gens_dir: PathBuf,
    metrics_file: PathBuf,
    gens: Mutex<Vec<Arc<Gen>>>,
    status: Mutex<Status>,
    building: tokio::sync::Mutex<()>,
}

impl Repo {
    pub(crate) async fn open(name: String, cfg: RepoCfg, state_dir: &Path, slots: u16) -> anyhow::Result<Self> {
        let gens_dir = state_dir.join(&name).join("gens");
        let jobs_dir = state_dir.join(&name).join("jobs");
        remove_tree(&jobs_dir).await?;
        fs::create_dir_all(&jobs_dir)?;
        fs::create_dir_all(&gens_dir)?;
        let gens = adopt(&gens_dir)?;
        let metrics_file = state_dir.join(&name).join("metrics.jsonl");
        let status = Status {
            state: if gens.is_empty() { State::Empty } else { State::Ready },
            sha: None,
            error: None,
            phases: Vec::new(),
        };
        Ok(Self {
            name,
            cfg,
            slots: Semaphore::new(usize::from(slots)),
            jobs_dir,
            gens_dir,
            metrics_file,
            gens: Mutex::new(gens),
            status: Mutex::new(status),
            building: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn cfg(&self) -> &RepoCfg {
        &self.cfg
    }

    pub(crate) fn metrics_file(&self) -> PathBuf {
        self.metrics_file.clone()
    }

    pub(crate) fn jobs_dir(&self) -> &Path {
        &self.jobs_dir
    }

    pub(crate) async fn slot(&self) -> anyhow::Result<SemaphorePermit<'_>> {
        Ok(self.slots.acquire().await?)
    }

    pub(crate) fn current(&self) -> Option<Arc<Gen>> {
        self.gens().last().cloned()
    }

    pub(crate) fn find(&self, sha: &str) -> Option<Arc<Gen>> {
        self.gens().iter().find(|generation| generation.sha == sha).cloned()
    }

    pub(crate) fn view(&self) -> View {
        let gens = self.gens().iter().map(|generation| generation.sha.clone()).collect::<Vec<_>>();
        View { status: self.status().clone(), current: gens.last().cloned(), gens }
    }

    pub(crate) async fn refresh(&self, keep: Duration) -> anyhow::Result<()> {
        let _building = self.building.lock().await;
        let head = self.remote_head().await?;
        if self.current().is_none_or(|current| current.sha != head) {
            self.set_status(State::Refreshing, &head, None, Vec::new());
            let mut clock = Clock::start();
            let built = self.build(&head, &mut clock).await;
            let fields = json!({ "repo": self.name, "sha": head, "ok": built.is_ok() });
            metrics::record(&self.metrics_file, "build", fields, &clock).await;
            let phases = clock.into_phases();
            match built {
                Ok(generation) => {
                    self.gens().push(Arc::new(generation));
                    self.set_status(State::Ready, &head, None, phases);
                }
                Err(err) => {
                    self.set_status(State::Failed, &head, Some(format!("{err:#}")), phases);
                    return Err(err);
                }
            }
        }
        self.prune(keep).await
    }

    async fn remote_head(&self) -> anyhow::Result<String> {
        let refname = format!("refs/heads/{}", self.cfg.branch);
        let out = checked(Command::new("git").args(["ls-remote", &self.cfg.url, &refname])).await?;
        let sha = out.split_whitespace().next().with_context(|| format!("{} has no {refname}", self.cfg.url))?;
        Ok(sha.to_owned())
    }

    async fn build(&self, sha: &str, clock: &mut Clock) -> anyhow::Result<Gen> {
        let dir = self.gens_dir.join(sha);
        let tmp = self.gens_dir.join(format!("{sha}.tmp"));
        remove_tree(&tmp).await?;
        remove_tree(&dir).await?;
        clock.mark("clean");
        if let Some(base) = self.current() {
            checked(Command::new("cp").args(["-a", "--reflink=auto"]).arg(&base.dir).arg(&tmp)).await?;
            clock.mark("copy");
            git(&tmp, &["fetch", "--quiet", "origin", sha]).await?;
            clock.mark("fetch");
        } else {
            checked(Command::new("git").args(["clone", "--quiet", &self.cfg.url]).arg(&tmp)).await?;
            clock.mark("clone");
        }
        git(&tmp, &["checkout", "--quiet", "--detach", sha]).await?;
        clock.mark("checkout");
        git(&tmp, &["submodule", "update", "--quiet", "--init", "--recursive"]).await?;
        clock.mark("submodules");
        let sandbox = Sandbox { repo: &self.name, tree: Tree::Build(&tmp) };
        let mut baseline = Baseline::new();
        for job in self.cfg.all_jobs() {
            let (out, found) = sandbox.diagnose(&job).await?;
            ensure!(out.ok() || !found.is_empty(), "{} failed on {sha}: {}", job.name, out.tail());
            clock.mark(format!("job:{}", job.name));
            baseline.insert(job.name, found);
        }
        rename(&tmp, &dir).await?;
        write(self.gens_dir.join(format!("{sha}.json")), to_vec(&baseline)?).await?;
        clock.mark("save");
        Ok(Gen { sha: sha.to_owned(), dir, baseline, born: SystemTime::now() })
    }

    async fn prune(&self, keep: Duration) -> anyhow::Result<()> {
        let mut doomed = Vec::new();
        {
            let mut gens = self.gens();
            let count = gens.len();
            let mut index = 0;
            gens.retain(|generation| {
                index += 1;
                let young = generation.born.elapsed().is_ok_and(|age| age < keep);
                let stays = index == count || young || Arc::strong_count(generation) > 1;
                if !stays {
                    doomed.push(generation.sha.clone());
                }
                stays
            });
        }
        for sha in doomed {
            remove_file(self.gens_dir.join(format!("{sha}.json"))).await?;
            remove_tree(&self.gens_dir.join(sha)).await?;
        }
        Ok(())
    }

    fn set_status(&self, state: State, sha: &str, error: Option<String>, phases: Vec<Phase>) {
        *self.status() = Status { state, sha: Some(sha.to_owned()), error, phases };
    }

    fn gens(&self) -> MutexGuard<'_, Vec<Arc<Gen>>> {
        self.gens.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug)]
pub(crate) struct Gen {
    sha: String,
    dir: PathBuf,
    baseline: Baseline,
    born: SystemTime,
}

impl Gen {
    pub(crate) fn sha(&self) -> &str {
        &self.sha
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn baseline(&self, job: &str) -> &[Diag] {
        self.baseline.get(job).map_or(&[], Vec::as_slice)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct View {
    status: Status,
    current: Option<String>,
    gens: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    #[default]
    Empty,
    Ready,
    Refreshing,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize)]
struct Status {
    state: State,
    sha: Option<String>,
    error: Option<String>,
    phases: Vec<Phase>,
}

pub(crate) async fn poll(repo: Arc<Repo>, every: Duration, keep: Duration) -> ! {
    loop {
        if let Err(err) = repo.refresh(keep).await {
            eprintln!("warmd: {}: {err:#}", repo.name);
        }
        sleep(every).await;
    }
}

pub(crate) async fn remove_tree(path: &Path) -> anyhow::Result<()> {
    if symlink_metadata(path).await.is_err() {
        return Ok(());
    }
    checked(Command::new("chmod").arg("-R").arg("u+rwX").arg(path)).await?;
    checked(Command::new("rm").arg("-rf").arg(path)).await?;
    Ok(())
}

fn adopt(gens_dir: &Path) -> anyhow::Result<Vec<Arc<Gen>>> {
    let mut gens = Vec::new();
    for entry in fs::read_dir(gens_dir)? {
        let path = entry?.path();
        let Some(sha) = path.file_stem().and_then(|stem| stem.to_str()) else { continue };
        let dir = gens_dir.join(sha);
        if path.extension().is_none_or(|ext| ext != "json") || !dir.is_dir() {
            continue;
        }
        let baseline = from_slice(&fs::read(&path)?)?;
        let born = fs::metadata(&path)?.modified()?;
        gens.push(Arc::new(Gen { sha: sha.to_owned(), dir, baseline, born }));
    }
    gens.sort_by_key(|generation| generation.born);
    Ok(gens)
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    checked(Command::new("git").arg("-C").arg(dir).args(args)).await
}

async fn checked(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.output().await.with_context(|| format!("starting {:?}", cmd.as_std()))?;
    if !out.status.success() {
        bail!("{:?} failed: {}", cmd.as_std(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
