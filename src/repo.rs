use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsStr,
    fs,
    io::ErrorKind,
    iter::once,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, bail};
use fiemap::fiemap;
use serde::Serialize;
use serde_json::{Value, from_slice, from_str, json, to_vec};
use tokio::{
    fs::{read_dir, read_to_string, remove_file, rename, symlink_metadata, write},
    process::Command,
    spawn, sync,
    sync::{OwnedMutexGuard, Semaphore, SemaphorePermit},
    task::spawn_blocking,
    time::sleep,
};

use crate::{
    DAY,
    config::{Job, RepoCfg},
    diag::{self, Diag},
    lanes,
    metrics::{self, Clock, Phase},
    ra::Tier1,
    sandbox::{Sandbox, Tree},
    stats::now_secs,
    sweep::Swept,
};

const SESSION_IDLE: Duration = Duration::from_mins(30);

pub(crate) type Baseline = BTreeMap<String, Vec<Diag>>;

pub(crate) type Broken = BTreeMap<String, String>;

type Parts = BTreeMap<&'static str, u64>;

#[derive(Debug)]
pub(crate) struct Repo {
    name: String,
    cfg: Mutex<Arc<RepoCfg>>,
    retired: AtomicBool,
    slots: Semaphore,
    slot_count: u32,
    jobs_dir: PathBuf,
    gens_dir: PathBuf,
    metrics_file: PathBuf,
    keep: Duration,
    gens: Mutex<Vec<Arc<Gen>>>,
    status: Mutex<Status>,
    building: sync::Mutex<()>,
    /// The main commit whose build last failed: the poll does not build it again until main moves or the config
    /// is reloaded (a manual refresh still does).
    failed_sha: Mutex<Option<String>>,
    sessions: Mutex<HashMap<String, Arc<sync::Mutex<()>>>>,
    session_views: Mutex<Vec<SessionView>>,
    cache_bytes: AtomicU64,
    tier1: Tier1,
    swept: AtomicBool,
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
        let sessions = keep_sessions(&jobs_dir).await?;
        fs::create_dir_all(&jobs_dir)?;
        fs::create_dir_all(&gens_dir)?;
        let cache = gens_dir.with_file_name("cache");
        for overlay in ["upper", "work"] {
            if let Err(err) = fs::remove_dir_all(cache.join(overlay))
                && err.kind() != ErrorKind::NotFound
            {
                eprintln!("warmd: {name}: removing the old {overlay} cache dir: {err}");
            }
        }
        fs::create_dir_all(cache.join("home"))?;
        let gens = adopt(&gens_dir)?;
        spawn(measure(gens.clone()));
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
            slot_count: u32::from(slots),
            jobs_dir,
            gens_dir,
            metrics_file,
            keep,
            gens: Mutex::new(gens),
            status: Mutex::new(status),
            building: sync::Mutex::new(()),
            failed_sha: Mutex::default(),
            sessions: Mutex::new(sessions),
            session_views: Mutex::default(),
            cache_bytes: AtomicU64::new(0),
            tier1: Tier1::default(),
            swept: AtomicBool::new(false),
        })
    }

    pub(crate) const fn tier1(&self) -> &Tier1 {
        &self.tier1
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn cfg(&self) -> Arc<RepoCfg> {
        Arc::clone(&self.cfg.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn set_cfg(&self, cfg: RepoCfg) {
        *self.cfg.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(cfg);
        self.failed_sha.lock().unwrap_or_else(PoisonError::into_inner).take();
    }

    fn failed_on(&self, sha: &str) -> bool {
        self.failed_sha.lock().unwrap_or_else(PoisonError::into_inner).as_deref() == Some(sha)
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    pub(crate) fn metrics_file(&self) -> PathBuf {
        self.metrics_file.clone()
    }

    pub(crate) async fn session(&self, name: &str) -> (PathBuf, OwnedMutexGuard<()>) {
        let key = session_key(name);
        let lock =
            Arc::clone(self.sessions.lock().unwrap_or_else(PoisonError::into_inner).entry(key.clone()).or_default());
        let guard = lock.lock_owned().await;
        (self.jobs_dir.join("sessions").join(key), guard)
    }

    fn try_session(&self, key: &str) -> Option<(PathBuf, OwnedMutexGuard<()>)> {
        let guard =
            Arc::clone(self.sessions.lock().unwrap_or_else(PoisonError::into_inner).entry(key.to_owned()).or_default())
                .try_lock_owned()
                .ok()?;
        Some((self.jobs_dir.join("sessions").join(key), guard))
    }

    async fn prune_sessions(&self, idle: Duration) -> anyhow::Result<()> {
        let live = self.gens().iter().map(|generation| generation.sha.clone()).collect::<Vec<_>>();
        let names = self.sessions.lock().unwrap_or_else(PoisonError::into_inner).keys().cloned().collect::<Vec<_>>();
        let mut views = Vec::with_capacity(names.len());
        for name in names {
            // A session in use (a check queued or running on it) is skipped, so a long check never holds up
            // this repo's main poll; it is looked at again next poll.
            let Some((dir, _guard)) = self.try_session(&name) else {
                views.extend(
                    self.session_views
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .iter()
                        .filter(|view| session_key(&view.name) == name)
                        .cloned(),
                );
                continue;
            };
            let meta = read_to_string(dir.join("meta.json")).await.ok().and_then(|text| from_str::<Value>(&text).ok());
            let stale = meta.as_ref().is_none_or(|meta| {
                let used = meta["used_at"].as_u64().unwrap_or_default();
                used + idle.as_secs() < now_secs() || !live.iter().any(|sha| meta["gen"] == sha.as_str())
            });
            if stale {
                remove_tree(&dir).await?;
                self.sessions.lock().unwrap_or_else(PoisonError::into_inner).remove(&name);
            } else if let Some(meta) = meta {
                views.push(SessionView::of(name, &meta, idle, live.last()));
            }
        }
        views.sort_by_key(|view| Reverse(view.bytes));
        *self.session_views.lock().unwrap_or_else(PoisonError::into_inner) = views;
        let cache = self.cache_dir();
        if symlink_metadata(&cache).await.is_ok() {
            self.cache_bytes.store(dir_bytes(&cache).await?, Ordering::Relaxed);
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

    pub(crate) fn cache_dir(&self) -> PathBuf {
        self.gens_dir.with_file_name("cache").join("home")
    }

    pub(crate) fn free_slots(&self) -> usize {
        self.slots.available_permits()
    }

    pub(crate) const fn slot_count(&self) -> u32 {
        self.slot_count
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
            .map(|(bucket, bytes)| Part {
                jobs: match *bucket {
                    "cargo" => names(&|job| !job.slapslop && !job.cmd.contains(" fmt")),
                    "clippy" => names(&|job| !job.slapslop && !job.test && !job.cmd.contains(" fmt")),
                    "test" => names(&|job| job.test),
                    "slapslop" => names(&|job| job.slapslop),
                    "release" => {
                        let release = names(&|job| job.cmd.contains("--release"));
                        if release.is_empty() { vec!["release".to_owned()] } else { release }
                    }
                    other => vec![other.to_owned()],
                },
                bytes: *bytes,
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

    /// When `sha` is main's head but not built yet (its build may be running), builds it first, so a check on the newest
    /// main runs on it instead of rebasing onto the build before (and starting its branch cold twice).
    pub(crate) async fn catch_up_to(&self, sha: &str) -> anyhow::Result<()> {
        if self.find(sha).is_some() {
            return Ok(());
        }
        if self.remote_head().await? == sha {
            self.refresh(self.keep, false).await?;
        }
        Ok(())
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
        View {
            status: self.status().clone(),
            current,
            gens,
            sessions: self.session_views.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            cache_bytes: Some(self.cache_bytes.load(Ordering::Relaxed)).filter(|&bytes| bytes > 0),
            tier1: self.tier1.view(),
        }
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

    pub(crate) async fn refresh(&self, keep: Duration, retry_failed: bool) -> anyhow::Result<()> {
        let _building = self.building.lock().await;
        let head = self.remote_head().await?;
        if self.current().is_none_or(|current| current.sha != head) && (retry_failed || !self.failed_on(&head)) {
            self.set_status(State::Refreshing, &head, None, Vec::new());
            let mut clock = Clock::start();
            let mut stats = BTreeMap::new();
            let built = metrics::with_commands(self.build(&head, &mut clock, &mut stats)).await;
            let fields = json!({ "repo": self.name, "sha": head, "ok": built.is_ok(), "jobs": stats });
            metrics::record(&self.metrics_file, "build", fields, &clock).await;
            let phases = clock.into_phases();
            match built {
                Ok(generation) => {
                    self.gens().push(Arc::new(generation));
                    spawn(measure(self.gens().clone()));
                    self.set_status(State::Ready, &head, None, phases);
                }
                Err(err) => {
                    *self.failed_sha.lock().unwrap_or_else(PoisonError::into_inner) = Some(head.clone());
                    self.set_status(State::Failed, &head, Some(format!("{err:#}")), phases);
                    if let Err(pruned) = self.prune(keep).await {
                        eprintln!("warmd: {}: pruning after a failed build: {pruned:#}", self.name);
                    }
                    return Err(err);
                }
            }
        } else if let Some(current) = self.current().filter(|current| current.sha == head) {
            self.rescan(&current).await?;
        }
        if let Some(current) = self.current() {
            if !self.swept.swap(true, Ordering::Relaxed) {
                let _all = self.slots.acquire_many(self.slot_count).await?;
                self.sweep(&current.dir, current.born).await;
            }
            self.tier1.follow(self, current, keep).await;
        }
        self.prune(keep).await
    }

    /// Re-runs main's slapslop jobs with a newly installed slapslop, so checks compare against its hits, not the old
    /// one's. Checks wait meanwhile (it holds every slot), and the branch layers go: their slapslop output is the old
    /// tool's.
    async fn rescan(&self, current: &Arc<Gen>) -> anyhow::Result<()> {
        let Some(tool) = installed_slapslop().await.filter(|tool| current.tool.as_deref() != Some(tool.as_str()))
        else {
            return Ok(());
        };
        let _all = self.slots.acquire_many(self.slot_count).await?;
        self.set_status(State::Refreshing, &current.sha, None, Vec::new());
        let mut clock = Clock::start();
        let cache = self.cache_dir();
        let sandbox = Sandbox { repo: &self.name, tree: Tree::Build { dir: &current.dir, cache: &cache } };
        let cfg = self.cfg();
        let mut baseline = current.baseline.clone();
        let mut broken = current.broken.clone();
        let mut stats = BTreeMap::new();
        let ran =
            metrics::with_commands(lanes::run(sandbox, cfg.slapslop_main_cmds(), &mut clock, |job, out, found| {
                broken.remove(&job.name);
                if !out.ok() && found.is_empty() {
                    broken.insert(job.name.clone(), out.tail());
                }
                stats.insert(
                    job.name.clone(),
                    json!({ "rebuilt": diag::rebuilt(&out.stdout), "cpu_secs": out.cpu_secs, "hits": found.len() }),
                );
                baseline.insert(job.name.clone(), found);
                Ok(())
            }))
            .await;
        let fields = json!({ "repo": self.name, "sha": current.sha, "ok": ran.is_ok(), "jobs": stats, "rescan": tool });
        metrics::record(&self.metrics_file, "build", fields, &clock).await;
        ran?;
        let sha = &current.sha;
        write(self.gens_dir.join(format!("{sha}.json")), to_vec(&baseline)?).await?;
        write(self.gens_dir.join(format!("{sha}.broken.json")), to_vec(&broken)?).await?;
        write(self.gens_dir.join(format!("{sha}.tool")), &tool).await?;
        if let Some(generation) = self.gens().iter_mut().find(|generation| generation.sha == *sha) {
            *generation = Arc::new(Gen {
                sha: sha.clone(),
                dir: current.dir.clone(),
                baseline,
                broken,
                tool: Some(tool),
                born: current.born,
                bytes: AtomicU64::new(current.bytes.load(Ordering::Relaxed)),
                own_bytes: AtomicU64::new(current.own_bytes.load(Ordering::Relaxed)),
                parts: Mutex::new(current.parts.lock().unwrap_or_else(PoisonError::into_inner).clone()),
                own_parts: Mutex::new(current.own_parts.lock().unwrap_or_else(PoisonError::into_inner).clone()),
            });
        }
        remove_tree(&self.jobs_dir.join("sessions")).await?;
        self.session_views.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.set_status(State::Ready, sha, None, clock.into_phases());
        Ok(())
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

    async fn build(&self, sha: &str, clock: &mut Clock, stats: &mut BTreeMap<String, Value>) -> anyhow::Result<Gen> {
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
        let cache = self.cache_dir();
        let sandbox = Sandbox { repo: &self.name, tree: Tree::Build { dir: &tmp, cache: &cache } };
        let cfg = self.cfg();
        let mut baseline = Baseline::new();
        let mut broken = Broken::new();
        let started = SystemTime::now();
        lanes::run(sandbox, cfg.jobs.iter().map(|job| (job, job.main_cmd())).collect(), clock, |job, out, found| {
            if !out.ok() && found.is_empty() && !job.test {
                broken.insert(job.name.clone(), out.tail());
                return Ok(());
            }
            stats.insert(
                job.name.clone(),
                json!({ "rebuilt": diag::rebuilt(&out.stdout), "cpu_secs": out.cpu_secs, "hits": found.len() }),
            );
            baseline.insert(job.name.clone(), found);
            Ok(())
        })
        .await?;
        // One job broken on main (a lint the installed slapslop lacks, say) leaves the others' baseline usable:
        // checks skip it and say so. Only a main where every job broke fails.
        if baseline.is_empty()
            && let Some((name, tail)) = broken.iter().next()
        {
            bail!("{name} failed on {sha}: {tail}");
        }
        self.sweep(&tmp, started).await;
        clock.mark("sweep");
        rename(&tmp, &dir).await?;
        write(self.gens_dir.join(format!("{sha}.json")), to_vec(&baseline)?).await?;
        write(self.gens_dir.join(format!("{sha}.broken.json")), to_vec(&broken)?).await?;
        let tool = installed_slapslop().await;
        write(self.gens_dir.join(format!("{sha}.tool")), tool.clone().unwrap_or_default()).await?;
        clock.mark("save");
        Ok(Gen {
            sha: sha.to_owned(),
            dir,
            baseline,
            broken,
            tool,
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
            for extra in [format!("{sha}.broken.json"), format!("{sha}.tool")] {
                let file = self.gens_dir.join(extra);
                if file.exists() {
                    remove_file(file).await?;
                }
            }
            remove_tree(&self.gens_dir.join(sha)).await?;
        }
        let owned = self.gens().iter().map(|generation| generation.sha.clone()).collect::<HashSet<_>>();
        let mut entries = read_dir(&self.gens_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if owned.contains(entry.file_name().to_string_lossy().split('.').next().unwrap_or_default()) {
                continue;
            }
            if entry.file_type().await?.is_dir() {
                remove_tree(&entry.path()).await?;
            } else {
                remove_file(entry.path()).await?;
            }
        }
        Ok(())
    }

    async fn sweep(&self, tree: &Path, built_at: SystemTime) {
        let (target, name) = (tree.join("target"), self.name.clone());
        match spawn_blocking(move || Swept::sweep(&target, built_at.checked_sub(DAY).unwrap_or(built_at))).await {
            Ok(Ok(swept)) if swept.units + swept.incremental > 0 => eprintln!(
                "warmd: {name}: removed {} unused build units and {} incremental dirs",
                swept.units, swept.incremental
            ),
            Ok(Ok(_)) => {}
            Ok(Err(err)) => eprintln!("warmd: {name}: sweeping the target dir: {err:#}"),
            Err(err) => eprintln!("warmd: {name}: sweeping the target dir: {err}"),
        }
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
    /// Jobs that failed on this main without findings, with their output's tail.
    broken: Broken,
    /// The slapslop that made the baseline (`cargo-slapslop --version`): another one rescans main.
    tool: Option<String>,
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

    pub(crate) const fn born(&self) -> SystemTime {
        self.born
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn baseline(&self, job: &str) -> &[Diag] {
        self.baseline.get(job).map_or(&[], Vec::as_slice)
    }

    pub(crate) const fn broken(&self) -> &Broken {
        &self.broken
    }

    pub(crate) const fn hits(&self) -> &Baseline {
        &self.baseline
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct View {
    status: Status,
    current: Option<String>,
    gens: Vec<GenView>,
    sessions: Vec<SessionView>,
    cache_bytes: Option<u64>,
    tier1: Value,
}

#[derive(Debug, Clone, Serialize)]
struct SessionView {
    name: String,
    bytes: Option<u64>,
    idle_secs: u64,
    expires_in_secs: u64,
    on_main: bool,
}

impl SessionView {
    fn of(name: String, meta: &Value, idle: Duration, main: Option<&String>) -> Self {
        let idle_secs = now_secs().saturating_sub(meta["used_at"].as_u64().unwrap_or_default());
        Self {
            name: meta["session"].as_str().map_or(name, str::to_owned),
            bytes: meta["bytes"].as_u64(),
            idle_secs,
            expires_in_secs: idle.as_secs().saturating_sub(idle_secs),
            on_main: main.is_some_and(|sha| meta["gen"] == sha.as_str()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct Part {
    jobs: Vec<String>,
    bytes: u64,
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
        if let Err(err) = repo.refresh(keep, false).await {
            eprintln!("warmd: {}: {err:#}", repo.name);
        }
        if let Err(err) = repo.prune_sessions(SESSION_IDLE).await {
            eprintln!("warmd: {} sessions: {err:#}", repo.name);
        }
        sleep(every).await;
    }
}

pub(crate) async fn dir_bytes(path: &Path) -> anyhow::Result<u64> {
    Ok(checked_quiet(Command::new("du").args([OsStr::new("-sb"), path.as_os_str()]))
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
    checked_quiet(Command::new("chmod").arg("-R").arg("u+rwX").arg(path)).await?;
    checked_quiet(Command::new("rm").arg("-rf").arg(path)).await?;
    Ok(())
}

pub(crate) async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    checked(Command::new("git").arg("-C").arg(dir).args(args)).await
}

/// Clears a repo's jobs dir after a restart but keeps its sessions (a branch's warm build), so a deploy costs no warmth.
async fn keep_sessions(jobs_dir: &Path) -> anyhow::Result<HashMap<String, Arc<sync::Mutex<()>>>> {
    let mut kept = HashMap::new();
    if symlink_metadata(jobs_dir).await.is_err() {
        return Ok(kept);
    }
    for entry in fs::read_dir(jobs_dir)? {
        let path = entry?.path();
        if path.file_name() != Some(OsStr::new("sessions")) {
            remove_tree(&path).await?;
        }
    }
    let sessions = jobs_dir.join("sessions");
    if symlink_metadata(&sessions).await.is_err() {
        return Ok(kept);
    }
    for entry in fs::read_dir(sessions)? {
        let dir = entry?.path();
        match read_to_string(dir.join("meta.json"))
            .await
            .ok()
            .and_then(|text| from_str::<Value>(&text).ok())
            .and_then(|meta| meta["session"].as_str().map(str::to_owned))
        {
            Some(name) => {
                kept.insert(session_key(&name), Arc::default());
            }
            None => remove_tree(&dir).await?,
        }
    }
    Ok(kept)
}

/// The session dir and lock of a branch: two branches that differ only in punctuation share one, never a dir
/// with two locks.
fn session_key(name: &str) -> String {
    name.bytes().map(|byte| if byte.is_ascii_alphanumeric() { char::from(byte) } else { '_' }).collect()
}

/// The installed slapslop's version line, none when it does not run.
async fn installed_slapslop() -> Option<String> {
    let out = Command::new("cargo-slapslop").arg("--version").output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|line| !line.is_empty())
}

async fn measure(gens: Vec<Arc<Gen>>) {
    match spawn_blocking(move || measure_blocks(&gens)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("warmd: measuring generations: {err:#}"),
        Err(err) => eprintln!("warmd: measuring generations: {err}"),
    }
}

fn measure_blocks(gens: &[Arc<Gen>]) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for (index, generation) in gens.iter().rev().enumerate() {
        let mut parts = Parts::new();
        let (mut units, macros) = (HashMap::new(), proc_macros(&generation.dir.join("target/debug/build"))?);
        walk_files(&generation.dir, &mut |relative, path| {
            let part = part_of(&generation.dir, &mut units, &macros, relative)?;
            for extent in fiemap(path)? {
                match extent {
                    Ok(extent) => {
                        if seen.insert(extent.fe_physical) || extent.fe_physical == 0 {
                            *parts.entry(part).or_default() += extent.fe_length;
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::Unsupported => {
                        *parts.entry(part).or_default() += fs::metadata(path)?.len();
                        break;
                    }
                    Err(err) => return Err(err).with_context(|| format!("mapping {}", path.display())),
                }
            }
            Ok(())
        })?;
        let total = parts.values().sum();
        let (bytes, target) = if index == 0 {
            (&generation.bytes, &generation.parts)
        } else {
            (&generation.own_bytes, &generation.own_parts)
        };
        bytes.store(total, Ordering::Relaxed);
        *target.lock().unwrap_or_else(PoisonError::into_inner) = parts;
    }
    Ok(())
}

fn walk_files(root: &Path, visit: &mut dyn FnMut(&Path, &Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                let path = entry.path();
                visit(path.strip_prefix(root).unwrap_or(&path), &path)?;
            }
        }
    }
    Ok(())
}

fn part_of(
    root: &Path,
    units: &mut HashMap<PathBuf, &'static str>,
    macros: &HashSet<String>,
    relative: &Path,
) -> anyhow::Result<&'static str> {
    if relative.starts_with("target/slapslop") {
        return Ok("slapslop");
    }
    if relative.starts_with("target/release") {
        return Ok("release");
    }
    if !relative.starts_with("target") {
        return Ok("source");
    }
    let parts = relative.iter().collect::<Vec<_>>();
    Ok(match parts.get(2).and_then(|part| part.to_str()) {
        Some("incremental")
            if parts
                .get(3)
                .and_then(|dir| dir.to_str()?.rsplit_once('-'))
                .is_some_and(|(name, _)| macros.contains(name)) =>
        {
            "cargo"
        }
        Some("build")
            if parts.len() > 5 && !root.join(parts[..5].iter().collect::<PathBuf>()).join("fingerprint").is_dir() =>
        {
            "cargo"
        }
        Some("build" | "incremental") if parts.len() > 5 => {
            let unit = parts[..5].iter().collect::<PathBuf>();
            if let Some(kind) = units.get(&unit) {
                kind
            } else {
                let kind = unit_kind(&root.join(&unit))?;
                units.insert(unit, kind);
                kind
            }
        }
        Some("deps") => match relative.extension().and_then(|ext| ext.to_str()) {
            Some("rmeta" | "d") => "clippy",
            Some("so") => "cargo",
            Some(_) | None => "test",
        },
        Some(_) | None => "cargo",
    })
}

fn proc_macros(build: &Path) -> anyhow::Result<HashSet<String>> {
    let mut macros = file_names(&build.with_file_name("deps"))?
        .iter()
        .filter_map(|file| Some(file.strip_suffix(".so")?.strip_prefix("lib")?.rsplit_once('-')?.0.to_owned()))
        .collect::<HashSet<_>>();
    for package in file_names(build)? {
        for unit in file_names(&build.join(&package))? {
            for file in file_names(&build.join(&package).join(unit).join("out"))? {
                if let Some(name) = file.strip_suffix(".so").and_then(|stem| stem.strip_prefix("lib")?.rsplit_once('-'))
                {
                    macros.insert(name.0.to_owned());
                }
            }
        }
    }
    Ok(macros)
}

fn file_names(dir: &Path) -> anyhow::Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    fs::read_dir(dir)?.map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned())).collect()
}

fn unit_kind(dir: &Path) -> anyhow::Result<&'static str> {
    let fingerprints = file_names(&dir.join("fingerprint"))?;
    if fingerprints.iter().any(|name| name.starts_with("build-script") || name.starts_with("run-build-script")) {
        return Ok("cargo");
    }
    let files = file_names(&if fingerprints.is_empty() { dir.to_owned() } else { dir.join("out") })?;
    Ok(if files.iter().any(|name| Path::new(name).extension().is_some_and(|ext| ext == "so")) {
        "cargo"
    } else if files.iter().any(|name| {
        Path::new(name).extension().is_none_or(|ext| ["rlib", "o", "dwo", "a"].iter().any(|code| ext == *code))
    }) {
        "test"
    } else {
        "clippy"
    })
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
        let broken_file = gens_dir.join(format!("{sha}.broken.json"));
        let broken = if broken_file.exists() { from_slice(&fs::read(&broken_file)?)? } else { Broken::new() };
        let tool = fs::read_to_string(gens_dir.join(format!("{sha}.tool"))).ok().filter(|tool| !tool.is_empty());
        let born = fs::metadata(&dir)?.created()?;
        gens.push(Arc::new(Gen {
            sha: sha.to_owned(),
            dir,
            baseline,
            broken,
            tool,
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

async fn checked(cmd: &mut Command) -> anyhow::Result<String> {
    let std = cmd.as_std();
    metrics::note_command(
        &once(std.get_program()).chain(std.get_args()).map(OsStr::to_string_lossy).collect::<Vec<_>>().join(" "),
    );
    checked_quiet(cmd).await
}

async fn checked_quiet(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.output().await.with_context(|| format!("starting {:?}", cmd.as_std()))?;
    if !out.status.success() {
        bail!("{:?} failed: {}", cmd.as_std(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
