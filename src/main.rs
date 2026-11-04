use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
    env,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
    serve,
};
use serde::Serialize;
use serde_json::{Value, json, to_value};
use tokio::{
    net::TcpListener,
    process::Command,
    signal::unix::{SignalKind, signal},
    spawn,
    time::sleep,
};
use uuid::Uuid;

use crate::{
    check::{Outcome, Request},
    config::{Config, Profile},
    metrics::{Clock, Phase, RETAIN_SECS, with_commands},
    repo::{Repo, View},
    stats::{Machine, now_secs},
};

mod check;
mod config;
mod diag;
mod hits_view;
mod lanes;
mod metrics;
mod repo;
mod sandbox;
mod stats;

const SECS_PER_HOUR: u64 = 3600;

const PRUNE_EVERY: Duration = Duration::from_hours(1);

const RECENT_PER_REPO: usize = 100;

const RECENT_JOBS: usize = 25;

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
}

#[derive(Debug, Clone, Serialize)]
struct Live {
    repo: String,
    kind: &'static str,
    branch: Option<String>,
    base: String,
    started: u64,
    running_since: Option<u64>,
    request: Value,
}

#[derive(Debug)]
struct App {
    repos: Mutex<BTreeMap<String, Arc<Repo>>>,
    full: Profile,
    jobs: Mutex<HashMap<Uuid, (u64, Job)>>,
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
    let sampler = Arc::clone(&app);
    spawn(async move { sampler.machine.sample_forever().await });
    let router = Router::new()
        .route("/", get(page))
        .route("/stats", get(stats_view))
        .route("/check", post(submit))
        .route("/slapslop/baseline", post(slapslop_baseline))
        .route("/jobs/{id}", get(job))
        .route("/repos/{name}", get(repo_view))
        .route("/repos/{name}/refresh", post(refresh))
        .route("/repos/{name}/hits", get(hits))
        .route("/repos/{name}/hits/view", get(hits_view))
        .with_state(app);
    serve(TcpListener::bind(&config.listen).await.with_context(|| format!("binding {}", config.listen))?, router)
        .await?;
    Ok(())
}

async fn submit(State(app): State<Arc<App>>, Json(req): Json<Request>) -> Reply<Value> {
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
        request: req.summary(),
    };
    app.live.lock().unwrap_or_else(PoisonError::into_inner).insert(id, live);
    let (app, repo) = (Arc::clone(app), Arc::clone(repo));
    spawn(async move {
        let layer = repo.jobs_dir().join(id.to_string());
        let running = || {
            app.set(id, Job::Running { progress: None });
            if let Some(entry) = app.live.lock().unwrap_or_else(PoisonError::into_inner).get_mut(&id) {
                entry.running_since = Some(now_secs());
            }
        };
        let progress = |value| app.set(id, Job::Running { progress: Some(value) });
        let (outcome, phases) = with_commands(check::run(&repo, &app.full, req, &layer, running, progress)).await;
        app.live.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        app.finish(id, outcome.map(Done::Check), phases);
    });
    id
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
    Json(
        json!({ "machine": machine, "repos": repos, "recent": recent, "jobs": counts, "live": live, "building": builds }),
    )
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
