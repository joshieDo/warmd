#![forbid(unsafe_code)]

use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
    env,
    io::ErrorKind,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::{Context as _, anyhow};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
    routing::{delete, get, post},
    serve,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, from_str, json, to_value};
use tokio::{
    fs::{read_to_string, write},
    net::TcpListener,
    process::Command,
    select,
    signal::unix::{SignalKind, signal},
    spawn,
    sync::{Semaphore, SemaphorePermit},
    time::sleep,
};
use uuid::Uuid;

use crate::{
    balance::{Balancer, Pending, Source},
    check::{Outcome, Request},
    config::{Config, Job as JobCfg, Profile},
    metrics::{Clock, Phase, RETAIN_SECS, with_commands},
    repo::{Repo, View},
    stats::{Machine, now_secs},
};

mod balance;
mod check;
mod config;
mod diag;
mod hits_view;
mod lanes;
mod metrics;
mod remote;
mod repo;
mod sandbox;
mod stats;

const SECS_PER_HOUR: u64 = 3600;

const PRUNE_EVERY: Duration = Duration::from_hours(1);

const RECENT_PER_REPO: usize = 100;

const RECENT_JOBS: usize = 25;

const DISPATCH_RECHECK: Duration = Duration::from_secs(1);

const REMOTES_FILE: &str = "remotes.json";

static RUNNING: Semaphore = Semaphore::const_new(256);

type Reply<T> = Result<Json<T>, (StatusCode, String)>;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Job {
    Queued,
    Running { progress: Option<Value> },
    Done { result: Done, phases: Vec<Phase> },
    Failed { error: String, phases: Vec<Phase> },
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum Done {
    Check(Outcome),
    Refresh(View),
    Remote(Value),
}

#[derive(Debug, Clone, Serialize)]
struct Live {
    repo: String,
    kind: &'static str,
    branch: Option<String>,
    base: String,
    started: u64,
    running_since: Option<u64>,
    remote: Option<String>,
    request: Value,
}

#[derive(Debug)]
struct App {
    repos: Mutex<BTreeMap<String, Arc<Repo>>>,
    full: Profile,
    jobs: Mutex<HashMap<Uuid, (u64, Job)>>,
    balance: Balancer,
    lease: Duration,
    live: Mutex<HashMap<Uuid, Live>>,
    config_dir: PathBuf,
    keep: Duration,
    machine: Machine,
    state_dir: PathBuf,
}

impl App {
    fn repo(&self, name: &str) -> Result<Arc<Repo>, (StatusCode, String)> {
        self.repos().get(name).cloned().ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repo {name}")))
    }

    fn repos(&self) -> BTreeMap<String, Arc<Repo>> {
        self.repos.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set(&self, id: Uuid, job: Job) {
        let now = now_secs();
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        jobs.retain(|_, (at, _)| *at >= now.saturating_sub(RETAIN_SECS));
        jobs.insert(id, (now, job));
    }

    async fn reload(&self) -> anyhow::Result<()> {
        let config = Config::load(&self.config_dir)?;
        let poll_every = Duration::from_secs(config.poll_secs);
        let mut current = self.repos();
        for (name, cfg) in config.repos {
            if let Some(repo) = current.remove(&name) {
                repo.set_cfg(cfg);
                continue;
            }
            let repo = Arc::new(Repo::open(name.clone(), cfg, &self.state_dir, config.jobs_per_repo, self.keep).await?);
            spawn(repo::poll(Arc::clone(&repo), poll_every, self.keep));
            self.repos.lock().unwrap_or_else(PoisonError::into_inner).insert(name, repo);
        }
        for (name, repo) in current {
            repo.retire();
            self.repos.lock().unwrap_or_else(PoisonError::into_inner).remove(&name);
        }
        Ok(())
    }

    fn finish(&self, id: Uuid, result: anyhow::Result<Done>, phases: Vec<Phase>) {
        let job = match result {
            Ok(result) => Job::Done { result, phases },
            Err(err) => Job::Failed { error: format!("{err:#}"), phases },
        };
        self.set(id, job);
    }

    fn update_live(&self, id: Uuid, change: impl FnOnce(&mut Live)) {
        if let Some(entry) = self.live.lock().unwrap_or_else(PoisonError::into_inner).get_mut(&id) {
            change(entry);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddRemote {
    addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = env::args_os().nth(1).map_or_else(|| PathBuf::from("."), PathBuf::from);
    let config = Config::load(&dir)?;
    let keep = Duration::from_secs(config.keep_hours * SECS_PER_HOUR);
    let poll_every = Duration::from_secs(config.poll_secs);
    let mut repos = BTreeMap::new();
    for (name, cfg) in config.repos {
        let repo = Arc::new(Repo::open(name.clone(), cfg, &config.state_dir, config.jobs_per_repo, keep).await?);
        spawn(repo::poll(Arc::clone(&repo), poll_every, keep));
        repos.insert(name, repo);
    }
    let app = Arc::new(App {
        repos: Mutex::new(repos),
        full: Profile::full()?,
        config_dir: dir,
        jobs: Mutex::default(),
        balance: Balancer::new(load_remotes(&config.state_dir).await),
        lease: Duration::from_secs(config.lease_secs),
        live: Mutex::default(),
        keep,
        machine: Machine::default(),
        state_dir: config.state_dir.clone(),
    });
    let reloader = Arc::clone(&app);
    let mut hangups = signal(SignalKind::hangup())?;
    spawn(async move {
        while hangups.recv().await.is_some() {
            match reloader.reload().await {
                Ok(()) => eprintln!("warmd: reloaded repos.toml"),
                Err(err) => eprintln!("warmd: reload failed, keeping the old config: {err:#}"),
            }
        }
    });
    if let Some(remote) = env::var_os("SLAPSLOP_REMOTE_CACHE").filter(|dir| !dir.is_empty()) {
        spawn(prune_remote_forever(PathBuf::from(remote)));
    }
    spawn(dispatch_forever(Arc::clone(&app)));
    spawn(remote::poll_capacity_forever(Arc::clone(&app)));
    let sampler = Arc::clone(&app);
    spawn(async move { sampler.machine.sample_forever().await });
    let router = Router::new()
        .route("/", get(page))
        .route("/stats", get(stats_view))
        .route("/check", post(submit))
        .route("/test", post(submit_tests))
        .route("/slapslop/baseline", post(slapslop_baseline))
        .route("/jobs/{id}", get(job))
        .route("/repos/{name}", get(repo_view))
        .route("/repos/{name}/refresh", post(refresh))
        .route("/repos/{name}/hits", get(hits))
        .route("/repos/{name}/hits/view", get(hits_view))
        .route("/capacity", get(capacity))
        .route("/remotes", get(remotes_view).post(add_remote))
        .route("/remotes/{addr}", delete(remove_remote))
        .with_state(app);
    serve(TcpListener::bind(&config.listen).await.with_context(|| format!("binding {}", config.listen))?, router)
        .await?;
    Ok(())
}

async fn submit(State(app): State<Arc<App>>, Json(req): Json<Request>) -> Reply<Value> {
    if req.tests().is_some() {
        return Err((StatusCode::BAD_REQUEST, "tests go to POST /test".to_owned()));
    }
    let repo = app.repo(req.repo())?;
    if req.wants_full() {
        let (ci, full) = req.split_full();
        let id = start(&app, &repo, ci);
        let full_id = start(&app, &repo, full);
        return Ok(Json(json!({ "id": id, "full_id": full_id })));
    }
    Ok(Json(json!({ "id": start(&app, &repo, req) })))
}

fn start(app: &Arc<App>, repo: &Arc<Repo>, req: Request) -> Uuid {
    let id = Uuid::new_v4();
    app.set(id, Job::Queued);
    let live = Live {
        repo: repo.name().to_owned(),
        kind: req.kind(),
        branch: req.branch().map(str::to_owned),
        base: req.base_sha().to_owned(),
        started: now_secs(),
        running_since: None,
        remote: None,
        request: req.summary(),
    };
    app.live.lock().unwrap_or_else(PoisonError::into_inner).insert(id, live);
    app.balance.enqueue(Pending::new(id, req));
    id
}

async fn dispatch_forever(app: Arc<App>) -> ! {
    loop {
        let woken = app.balance.wake().notified();
        while let Some((job, source)) = app.balance.next(&|name: &str, started: u32| {
            app.repo(name).is_ok_and(|repo| started < repo.slot_count() && repo.free_slots() > 0)
        }) {
            let Ok(permit) = RUNNING.acquire().await else { continue };
            spawn(run(Arc::clone(&app), job, source, permit));
        }
        select! {
            () = woken => {}
            () = sleep(DISPATCH_RECHECK) => {}
        }
    }
}

async fn run(app: Arc<App>, job: Pending, source: Source, permit: SemaphorePermit<'static>) {
    let repo_name = job.req().repo().to_owned();
    match &source {
        Source::Local => match app.repo(&repo_name) {
            Ok(repo) => run_local(&app, &repo, job.id(), job.into_req()).await,
            Err((_, why)) => {
                app.live.lock().unwrap_or_else(PoisonError::into_inner).remove(&job.id());
                app.finish(job.id(), Err(anyhow!(why)), Vec::new());
            }
        },
        Source::Remote(addr) => run_remote(&app, addr, job).await,
    }
    app.balance.ended(&repo_name, &source);
    drop(permit);
}

async fn run_local(app: &Arc<App>, repo: &Arc<Repo>, id: Uuid, req: Request) {
    let layer = repo.jobs_dir().join(id.to_string());
    let running = || {
        app.set(id, Job::Running { progress: None });
        app.update_live(id, |entry| entry.running_since = Some(now_secs()));
    };
    let progress = |value| app.set(id, Job::Running { progress: Some(value) });
    let (outcome, phases) = with_commands(check::run(repo, &app.full, req, &layer, running, progress)).await;
    app.live.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
    app.finish(id, outcome.map(Done::Check), phases);
}

async fn run_remote(app: &Arc<App>, addr: &str, job: Pending) {
    let id = job.id();
    app.set(id, Job::Running { progress: None });
    app.update_live(id, |entry| {
        entry.running_since = Some(now_secs());
        entry.remote = Some(addr.to_owned());
    });
    let progress = |value| app.set(id, Job::Running { progress: Some(value) });
    let (result, phases) = match remote::run(app, addr, job.req(), app.lease, progress).await {
        remote::End::Done(result, phases) => (Ok(Done::Remote(result)), phases),
        remote::End::Failed(why, phases) => (Err(anyhow!(why)), phases),
        remote::End::Lost(why) => match app.balance.give_back(job, addr, &why) {
            None => {
                eprintln!("warmd: {id}: {why}; queued again");
                app.set(id, Job::Queued);
                app.update_live(id, |entry| {
                    entry.running_since = None;
                    entry.remote = None;
                });
                return;
            }
            Some(failed) => (Err(anyhow!(failed)), Vec::new()),
        },
    };
    app.live.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
    app.finish(id, result, phases);
}

async fn remotes_view(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.balance.view())
}

async fn add_remote(State(app): State<Arc<App>>, Json(body): Json<AddRemote>) -> Reply<Value> {
    let addr = body.addr.trim().trim_start_matches("http://").trim_end_matches('/').to_owned();
    if !addr.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok()) {
        return Err((StatusCode::BAD_REQUEST, format!("{addr:?} is not ip:port (or host:port)")));
    }
    let added = app.balance.add(&addr);
    save_remotes(&app).await?;
    Ok(Json(json!({ "addr": addr, "added": added })))
}

async fn remove_remote(State(app): State<Arc<App>>, Path(addr): Path<String>) -> Reply<Value> {
    let removed = app.balance.remove(&addr);
    save_remotes(&app).await?;
    Ok(Json(json!({ "addr": addr, "removed": removed })))
}

async fn save_remotes(app: &App) -> Result<(), (StatusCode, String)> {
    write(app.state_dir.join(REMOTES_FILE), json!(app.balance.remotes()).to_string())
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("saving the remotes: {err}")))
}

async fn load_remotes(state_dir: &FsPath) -> Vec<String> {
    match read_to_string(state_dir.join(REMOTES_FILE)).await {
        Ok(text) => from_str(&text).unwrap_or_else(|err| {
            eprintln!("warmd: {REMOTES_FILE}: {err}; starting without remotes");
            Vec::new()
        }),
        Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            eprintln!("warmd: {REMOTES_FILE}: {err}; starting without remotes");
            Vec::new()
        }
    }
}

async fn capacity(State(app): State<Arc<App>>) -> Json<Value> {
    let repos = app
        .repos()
        .into_iter()
        .map(|(name, repo)| (name, json!({ "built": repo.current().is_some(), "free": repo.free_slots() })))
        .collect::<Map<_, _>>();
    Json(json!({ "repos": repos }))
}

async fn submit_tests(State(app): State<Arc<App>>, Json(req): Json<Request>) -> Reply<Value> {
    let repo = app.repo(req.repo())?;
    let Some(tests) = req.tests() else {
        return Err((StatusCode::BAD_REQUEST, "name tests: {\"package\": .., \"names\": [..]}".to_owned()));
    };
    if let Some(problem) = tests.problem() {
        return Err((StatusCode::BAD_REQUEST, problem));
    }
    if !repo.cfg().jobs.iter().any(JobCfg::runs_named_tests) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{} has no test job with {{package}} and {{tests}} in its cmd", repo.name()),
        ));
    }
    Ok(Json(json!({ "id": start(&app, &repo, req.only_tests()) })))
}

async fn slapslop_baseline(app: State<Arc<App>>, Json(req): Json<Request>) -> Reply<Value> {
    submit(app, Json(req.baseline_only())).await
}

async fn refresh(State(app): State<Arc<App>>, Path(name): Path<String>) -> Reply<Value> {
    let repo = app.repo(&name)?;
    let id = Uuid::new_v4();
    app.set(id, Job::Running { progress: None });
    spawn(async move {
        let mut clock = Clock::start();
        let result = repo.refresh(app.keep, true).await.map(|()| Done::Refresh(repo.view()));
        clock.mark("refresh");
        app.finish(id, result, clock.into_phases());
    });
    Ok(Json(json!({ "id": id })))
}

async fn job(State(app): State<Arc<App>>, Path(id): Path<Uuid>) -> Reply<Job> {
    app.jobs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .map(|(_, job)| Json(job.clone()))
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no job {id}")))
}

async fn repo_view(State(app): State<Arc<App>>, Path(name): Path<String>) -> Reply<View> {
    Ok(Json(app.repo(&name)?.view()))
}

async fn hits(State(app): State<Arc<App>>, Path(name): Path<String>) -> Reply<Value> {
    let current =
        app.repo(&name)?.current().ok_or_else(|| (StatusCode::NOT_FOUND, format!("{name} has no main build yet")))?;
    Ok(Json(json!({ "sha": current.sha(), "jobs": current.hits() })))
}

async fn hits_view(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Query(filter): Query<hits_view::Filter>,
) -> Reply<Value> {
    let current =
        app.repo(&name)?.current().ok_or_else(|| (StatusCode::NOT_FOUND, format!("{name} has no main build yet")))?;
    to_value(hits_view::view(current.sha(), current.hits(), &filter))
        .map(Json)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

async fn page() -> Html<&'static str> {
    Html(include_str!("page.html"))
}

async fn stats_view(State(app): State<Arc<App>>) -> Json<Value> {
    let all = app.repos();
    let mut repos = Vec::with_capacity(all.len());
    let mut recent = Vec::new();
    for (name, repo) in &all {
        let history = repo.recent(RECENT_PER_REPO).await;
        let last_build = history.iter().rev().find(|record| record["kind"] == "build").cloned();
        let checks = history.iter().filter(|record| record["kind"] != "build").cloned().collect::<Vec<_>>();
        recent.extend(history.iter().filter(|record| record["kind"] == "build").cloned());
        recent.extend(checks.iter().cloned());
        let cfg = repo.cfg();
        let url = cfg.url.trim_end_matches(".git");
        repos
            .push(json!({ "name": name, "url": url, "view": repo.view(), "last_build": last_build, "checks": checks }));
    }
    recent.sort_by_key(|record| Reverse(record["at"].as_u64()));
    recent.truncate(RECENT_JOBS);
    let mut counts = BTreeMap::from([("queued", 0_u32), ("running", 0), ("done", 0), ("failed", 0)]);
    for (_, job) in app.jobs.lock().unwrap_or_else(PoisonError::into_inner).values() {
        let state = match job {
            Job::Queued => "queued",
            Job::Running { .. } => "running",
            Job::Done { .. } => "done",
            Job::Failed { .. } => "failed",
        };
        *counts.entry(state).or_default() += 1;
    }
    let machine = app.machine.view(&app.state_dir).await;
    let mut live = app.live.lock().unwrap_or_else(PoisonError::into_inner).values().cloned().collect::<Vec<_>>();
    live.sort_by_key(|job| job.started);
    let builds = all
        .iter()
        .filter_map(|(name, repo)| {
            repo.building().map(|(sha, since)| json!({ "repo": name, "sha": sha, "started": since }))
        })
        .collect::<Vec<_>>();
    Json(json!({
        "machine": machine,
        "repos": repos,
        "recent": recent,
        "jobs": counts,
        "live": live,
        "building": builds,
        "balance": app.balance.view(),
    }))
}

async fn prune_remote_forever(dir: PathBuf) -> ! {
    let days = (RETAIN_SECS / 86_400).to_string();
    loop {
        if let Err(err) =
            Command::new("find").arg(&dir).args(["-type", "f", "-mtime", &format!("+{days}"), "-delete"]).status().await
        {
            eprintln!("warmd: pruning {}: {err}", dir.display());
        }
        sleep(PRUNE_EVERY).await;
    }
}
