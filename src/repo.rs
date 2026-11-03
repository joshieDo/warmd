use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, bail, ensure};
use serde::Serialize;
use serde_json::{Value, from_slice, from_str, json, to_vec};
use tokio::{
    fs::{read_to_string, remove_file, rename, symlink_metadata, write},
    process::Command,
    spawn,
    sync::{OwnedMutexGuard, Semaphore, SemaphorePermit},
    time::sleep,
};

use crate::{
    config::{Job, RepoCfg},
    diag::Diag,
    metrics::{self, Clock, Phase},
    sandbox::{Sandbox, Tree},
    stats::now_secs,
};

const SESSION_IDLE: Duration = Duration::from_mins(30);

type Baseline = BTreeMap<String, Vec<Diag>>;

type Parts = BTreeMap<&'static str, u64>;

#[derive(Debug)]
pub(crate) struct Repo {
    name: String,
    cfg: Mutex<Arc<RepoCfg>>,
    retired: AtomicBool,
    slots: Semaphore,
    jobs_dir: PathBuf,
    gens_dir: PathBuf,
    metrics_file: PathBuf,
    keep: Duration,
    gens: Mutex<Vec<Arc<Gen>>>,
    status: Mutex<Status>,
    building: tokio::sync::Mutex<()>,
    sessions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Repo {
    pub(crate) async fn open(
        name: String,
        cfg: RepoCfg,
        state_dir: &Path,
        slots: u16,
        keep: Duration,
    ) -> anyhow::Result<Self> {
        let gens_dir = state_dir.join(&name).join("gens");
        let jobs_dir = state_dir.join(&name).join("jobs");
        remove_tree(&jobs_dir).await?;
        fs::create_dir_all(&jobs_dir)?;
        fs::create_dir_all(&gens_dir)?;
        let gens = adopt(&gens_dir)?;
        for generation in &gens {
            spawn(measure(Arc::clone(generation)));
        }
        for pair in gens.windows(2) {
            if let [older, newer] = pair {
                spawn(measure_own(Arc::clone(older), Arc::clone(newer)));
            }
        }
        let metrics_file = state_dir.join(&name).join("metrics.jsonl");
        let status = Status {
            state: if gens.is_empty() { State::Empty } else { State::Ready },
            sha: None,
            error: None,
            phases: Vec::new(),
            since: 0,
        };
        Ok(Self {
            name,
            cfg: Mutex::new(Arc::new(cfg)),
            retired: AtomicBool::new(false),
            slots: Semaphore::new(usize::from(slots)),
            jobs_dir,
            gens_dir,
            metrics_file,
            keep,
            gens: Mutex::new(gens),
            status: Mutex::new(status),
            building: tokio::sync::Mutex::new(()),
            sessions: Mutex::default(),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn cfg(&self) -> Arc<RepoCfg> {
        Arc::clone(&self.cfg.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn set_cfg(&self, cfg: RepoCfg) {
        *self.cfg.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(cfg);
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    pub(crate) fn metrics_file(&self) -> PathBuf {
        self.metrics_file.clone()
    }

    pub(crate) async fn session(&self, name: &str) -> (PathBuf, OwnedMutexGuard<()>) {
        let lock = Arc::clone(
            self.sessions.lock().unwrap_or_else(PoisonError::into_inner).entry(name.to_owned()).or_default(),
        );
        let guard = lock.lock_owned().await;
        let key = name
            .bytes()
            .map(|byte| if byte.is_ascii_alphanumeric() { char::from(byte) } else { '_' })
            .collect::<String>();
        (self.jobs_dir.join("sessions").join(key), guard)
    }

    async fn prune_sessions(&self, idle: Duration) -> anyhow::Result<()> {
        let live = self.gens().iter().map(|generation| generation.sha.clone()).collect::<Vec<_>>();
        let names = self.sessions.lock().unwrap_or_else(PoisonError::into_inner).keys().cloned().collect::<Vec<_>>();
        for name in names {
            let (dir, _guard) = self.session(&name).await;
            let meta = read_to_string(dir.join("meta.json")).await.ok().and_then(|text| from_str::<Value>(&text).ok());
            let stale = meta.as_ref().is_none_or(|meta| {
                let used = meta["used_at"].as_u64().unwrap_or_default();
                used + idle.as_secs() < now_secs() || !live.iter().any(|sha| meta["gen"] == sha.as_str())
            });
            if stale {
                remove_tree(&dir).await?;
                self.sessions.lock().unwrap_or_else(PoisonError::into_inner).remove(&name);
            }
        }
        Ok(())
    }

    pub(crate) fn building(&self) -> Option<(String, u64)> {
        let status = self.status();
        matches!(status.state, State::Refreshing).then(|| (status.sha.clone().unwrap_or_default(), status.since))
    }

    pub(crate) fn jobs_dir(&self) -> &Path {
        &self.jobs_dir
    }

    pub(crate) async fn slot(&self) -> anyhow::Result<SemaphorePermit<'_>> {
        Ok(self.slots.acquire().await?)
    }

    fn label(&self, parts: &Parts) -> Vec<Part> {
        let cfg = self.cfg();
        let jobs = &cfg.jobs;
        let names = |keep: &dyn Fn(&Job) -> bool| {
            jobs.iter().filter(|job| keep(job)).map(|job| job.name.clone()).collect::<Vec<_>>()
        };
        let mut labeled = parts
            .iter()
            .map(|(bucket, bytes)| {
                let names = match *bucket {
                    "cargo" => names(&|job| !job.slapslop && !job.cmd.contains(" fmt")),
                    "slapslop" => names(&|job| job.slapslop),
                    "release" => names(&|job| job.cmd.contains("--release")),
                    other => vec![other.to_owned()],
                };
                Part { jobs: names, bytes: *bytes }
            })
            .collect::<Vec<_>>();
        labeled.sort_by(|left, right| left.jobs.cmp(&right.jobs));
        labeled.dedup_by(|next, kept| {
            let same = next.jobs == kept.jobs;
            if same {
                kept.bytes += next.bytes;
            }
            same
        });
        let idle = names(&|job| !labeled.iter().any(|part| part.jobs.contains(&job.name)));
        if !idle.is_empty() {
            labeled.push(Part { jobs: idle, bytes: 0 });
        }
        labeled
    }

    pub(crate) fn current(&self) -> Option<Arc<Gen>> {
        self.gens().last().cloned()
    }

    pub(crate) fn find(&self, sha: &str) -> Option<Arc<Gen>> {
        self.gens().iter().find(|generation| generation.sha == sha).cloned()
    }

    pub(crate) fn view(&self) -> View {
        let all = self.gens();
        let newest = all.len().saturating_sub(1);
        let gens = all
            .iter()
            .enumerate()
            .map(|(index, generation)| {
                let age = generation.born.elapsed().unwrap_or_else(|ahead| {
                    eprintln!(
                        "warmd: {} generation {} is dated {:?} ahead",
                        self.name,
                        generation.sha,
                        ahead.duration()
                    );
                    Duration::ZERO
                });
                let expires_in_secs = (index < newest).then(|| self.keep.saturating_sub(age).as_secs());
                let bytes = Some(generation.bytes.load(Ordering::Relaxed)).filter(|&bytes| bytes > 0);
                let own_bytes =
                    (index < newest).then(|| generation.own_bytes.load(Ordering::Relaxed)).filter(|&bytes| bytes > 0);
                let source = if index < newest { &generation.own_parts } else { &generation.parts };
                let parts = self.label(&source.lock().unwrap_or_else(PoisonError::into_inner));
                GenView {
                    sha: generation.sha.clone(),
                    age_secs: age.as_secs(),
                    expires_in_secs,
                    bytes,
                    own_bytes,
                    parts,
                }
            })
            .collect::<Vec<GenView>>();
        let current = gens.last().map(|generation| generation.sha.clone());
        drop(all);
        View { status: self.status().clone(), current, gens }
    }

    pub(crate) async fn recent(&self, count: usize) -> Vec<Value> {
        let text = match read_to_string(&self.metrics_file).await {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                eprintln!("warmd: reading {}: {err}", self.metrics_file.display());
                return Vec::new();
            }
        };
        let lines = text.lines().collect::<Vec<_>>();
        lines.iter().skip(lines.len().saturating_sub(count)).filter_map(|line| from_str(line).ok()).collect()
    }

    pub(crate) async fn refresh(&self, keep: Duration) -> anyhow::Result<()> {
        let _building = self.building.lock().await;
        let head = self.remote_head().await?;
        if self.current().is_none_or(|current| current.sha != head) {
            self.set_status(State::Refreshing, &head, None, Vec::new());
            let mut clock = Clock::start();
            let built = metrics::with_commands(self.build(&head, &mut clock)).await;
            let fields = json!({ "repo": self.name, "sha": head, "ok": built.is_ok() });
            metrics::record(&self.metrics_file, "build", fields, &clock).await;
            let phases = clock.into_phases();
            match built {
                Ok(generation) => {
                    let generation = Arc::new(generation);
                    spawn(measure(Arc::clone(&generation)));
                    if let Some(previous) = self.current() {
                        spawn(measure_own(previous, Arc::clone(&generation)));
                    }
                    self.gens().push(generation);
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
        let cfg = self.cfg();
        let refname = format!("refs/heads/{}", cfg.branch);
        Ok((checked(Command::new("git").args(["ls-remote", &cfg.url, &refname]))
            .await?
            .split_whitespace()
            .next()
            .with_context(|| format!("{} has no {refname}", cfg.url))?)
        .to_owned())
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
            checked(Command::new("git").args(["clone", "--quiet", &self.cfg().url]).arg(&tmp)).await?;
            clock.mark("clone");
        }
        git(&tmp, &["checkout", "--quiet", "--detach", sha]).await?;
        clock.mark("checkout");
        git(&tmp, &["submodule", "update", "--quiet", "--init", "--recursive"]).await?;
        clock.mark("submodules");
        let cache = self.gens_dir.with_file_name("cache");
        fs::create_dir_all(cache.join("upper"))?;
        fs::create_dir_all(cache.join("work"))?;
        let sandbox = Sandbox { repo: &self.name, tree: Tree::Build { dir: &tmp, cache: &cache } };
        let mut baseline = Baseline::new();
        for job in self.cfg().jobs.clone() {
            let (out, found) = sandbox.diagnose(&job, &job.render(&job.cmd, None)).await?;
            ensure!(out.ok() || !found.is_empty(), "{} failed on {sha}: {}", job.name, out.tail());
            clock.mark(format!("job:{}", job.name));
            baseline.insert(job.name, found);
        }
        rename(&tmp, &dir).await?;
        write(self.gens_dir.join(format!("{sha}.json")), to_vec(&baseline)?).await?;
        clock.mark("save");
        Ok(Gen {
            sha: sha.to_owned(),
            dir,
            baseline,
            born: SystemTime::now(),
            bytes: AtomicU64::new(0),
            own_bytes: AtomicU64::new(0),
            parts: Mutex::default(),
            own_parts: Mutex::default(),
        })
    }

    async fn prune(&self, keep: Duration) -> anyhow::Result<()> {
        let mut doomed = Vec::new();
        {
            let mut gens = self.gens();
            let count = gens.len();
            let mut index = 0;
            gens.retain(|generation| {
                index += 1;
                let stays = index == count
                    || (generation.born.elapsed().is_ok_and(|age| age < keep))
                    || Arc::strong_count(generation) > 1;
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
        *self.status() = Status { state, sha: Some(sha.to_owned()), error, phases, since: now_secs() };
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
    bytes: AtomicU64,
    own_bytes: AtomicU64,
    parts: Mutex<Parts>,
    own_parts: Mutex<Parts>,
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
    gens: Vec<GenView>,
}

#[derive(Debug, Clone, Serialize)]
struct Part {
    jobs: Vec<String>,
    bytes: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct FileMeta {
    size: u64,
    mtime: String,
}

#[derive(Debug, Clone, Serialize)]
struct GenView {
    sha: String,
    age_secs: u64,
    expires_in_secs: Option<u64>,
    bytes: Option<u64>,
    own_bytes: Option<u64>,
    parts: Vec<Part>,
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
    #[serde(skip)]
    since: u64,
}

pub(crate) async fn poll(repo: Arc<Repo>, every: Duration, keep: Duration) {
    while !repo.retired.load(Ordering::Relaxed) {
        if let Err(err) = repo.refresh(keep).await {
            eprintln!("warmd: {}: {err:#}", repo.name);
        }
        if let Err(err) = repo.prune_sessions(SESSION_IDLE).await {
            eprintln!("warmd: {} sessions: {err:#}", repo.name);
        }
        sleep(every).await;
    }
}

pub(crate) async fn dir_bytes(path: &Path) -> anyhow::Result<u64> {
    Ok(checked(Command::new("du").arg("-sb").arg(path))
        .await?
        .split_whitespace()
        .next()
        .context("du printed nothing")?
        .parse()?)
}

pub(crate) async fn remove_tree(path: &Path) -> anyhow::Result<()> {
    if symlink_metadata(path).await.is_err() {
        return Ok(());
    }
    checked(Command::new("chmod").arg("-R").arg("u+rwX").arg(path)).await?;
    checked(Command::new("rm").arg("-rf").arg(path)).await?;
    Ok(())
}

async fn measure_own(older: Arc<Gen>, newer: Arc<Gen>) {
    match own_parts(&older.dir, &newer.dir).await {
        Ok(parts) => {
            older.own_bytes.store(parts.values().sum(), Ordering::Relaxed);
            *older.own_parts.lock().unwrap_or_else(PoisonError::into_inner) = parts;
        }
        Err(err) => eprintln!("warmd: comparing {} with {}: {err:#}", older.sha, newer.sha),
    }
}

async fn own_parts(older: &Path, newer: &Path) -> anyhow::Result<Parts> {
    let (old_files, new_files) = (file_index(older).await?, file_index(newer).await?);
    Ok(bucketize(old_files.iter().filter(|(path, meta)| new_files.get(*path) != Some(*meta))))
}

fn bucketize<'a>(files: impl Iterator<Item = (&'a String, &'a FileMeta)>) -> Parts {
    let mut parts = Parts::new();
    for (path, meta) in files {
        *parts
            .entry(if path.starts_with("target/slapslop/") {
                "slapslop"
            } else if path.starts_with("target/release/") {
                "release"
            } else if path.starts_with("target/") {
                "cargo"
            } else {
                "source"
            })
            .or_default() += meta.size;
    }
    parts
}

async fn file_index(dir: &Path) -> anyhow::Result<HashMap<String, FileMeta>> {
    Ok(checked(Command::new("find").arg(dir).args(["-type", "f", "-printf", "%P\\t%s\\t%T@\\n"]))
        .await?
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let path = fields.next()?.to_owned();
            Some((path, FileMeta { size: fields.next()?.parse().ok()?, mtime: fields.next()?.to_owned() }))
        })
        .collect())
}

async fn measure(generation: Arc<Gen>) {
    match file_index(&generation.dir).await {
        Ok(files) => {
            let parts = bucketize(files.iter());
            generation.bytes.store(parts.values().sum(), Ordering::Relaxed);
            *generation.parts.lock().unwrap_or_else(PoisonError::into_inner) = parts;
        }
        Err(err) => eprintln!("warmd: measuring {}: {err:#}", generation.dir.display()),
    }
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
        gens.push(Arc::new(Gen {
            sha: sha.to_owned(),
            dir,
            baseline,
            born,
            bytes: AtomicU64::new(0),
            own_bytes: AtomicU64::new(0),
            parts: Mutex::default(),
            own_parts: Mutex::default(),
        }));
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
