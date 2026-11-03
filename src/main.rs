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
    extract::{Path, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
    serve,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, spawn};
use uuid::Uuid;

use crate::{
    check::{Outcome, Request},
    config::Profile,
    metrics::{Clock, Phase},
    repo::{Repo, View},
    stats::Machine,
};

mod check;
mod config;
mod diag;
mod metrics;
mod repo;
mod sandbox;
mod stats;

const SECS_PER_HOUR: u64 = 3600;

const RECENT_PER_REPO: usize = 30;

const RECENT_JOBS: usize = 25;

type Reply<T> = Result<Json<T>, (StatusCode, String)>;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Job {
    Queued,
    Running,
    Done { result: Done, phases: Vec<Phase> },
    Failed { error: String, phases: Vec<Phase> },
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum Done {
    Check(Outcome),
    Refresh(View),
}

#[derive(Debug)]
struct App {
    repos: BTreeMap<String, Arc<Repo>>,
    profiles: BTreeMap<String, Profile>,
    jobs: Mutex<HashMap<Uuid, Job>>,
    keep: Duration,
    machine: Machine,
    state_dir: PathBuf,
}

impl App {
    fn repo(&self, name: &str) -> Result<Arc<Repo>, (StatusCode, String)> {
        self.repos.get(name).cloned().ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repo {name}")))
    }

    fn set(&self, id: Uuid, job: Job) {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner).insert(id, job);
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
    let (config, profiles) = config::load(&dir)?;
    let keep = Duration::from_secs(config.keep_hours * SECS_PER_HOUR);
    let poll_every = Duration::from_secs(config.poll_secs);
    let mut repos = BTreeMap::new();
    for (name, cfg) in config.repos {
        let repo = Arc::new(Repo::open(name.clone(), cfg, &config.state_dir, config.jobs_per_repo, keep).await?);
        spawn(repo::poll(Arc::clone(&repo), poll_every, keep));
        repos.insert(name, repo);
    }
    let app = Arc::new(App {
        repos,
        profiles,
        jobs: Mutex::default(),
        keep,
        machine: Machine::default(),
        state_dir: config.state_dir.clone(),
    });
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
        .with_state(app);
    let listener = TcpListener::bind(&config.listen).await.with_context(|| format!("binding {}", config.listen))?;
    serve(listener, router).await?;
    Ok(())
}

async fn submit(State(app): State<Arc<App>>, Json(req): Json<Request>) -> Reply<Value> {
    let repo = app.repo(req.repo())?;
    let id = Uuid::new_v4();
    app.set(id, Job::Queued);
    spawn(async move {
        let layer = repo.jobs_dir().join(id.to_string());
        let (outcome, phases) = check::run(&repo, &app.profiles, req, &layer, || app.set(id, Job::Running)).await;
        app.finish(id, outcome.map(Done::Check), phases);
    });
    Ok(Json(json!({ "id": id })))
}

async fn slapslop_baseline(app: State<Arc<App>>, Json(req): Json<Request>) -> Reply<Value> {
    submit(app, Json(req.baseline_only())).await
}

async fn refresh(State(app): State<Arc<App>>, Path(name): Path<String>) -> Reply<Value> {
    let repo = app.repo(&name)?;
    let id = Uuid::new_v4();
    app.set(id, Job::Running);
    spawn(async move {
        let mut clock = Clock::start();
        let result = repo.refresh(app.keep).await.map(|()| Done::Refresh(repo.view()));
        clock.mark("refresh");
        app.finish(id, result, clock.into_phases());
    });
    Ok(Json(json!({ "id": id })))
}

async fn job(State(app): State<Arc<App>>, Path(id): Path<Uuid>) -> Reply<Job> {
    let jobs = app.jobs.lock().unwrap_or_else(PoisonError::into_inner);
    jobs.get(&id).cloned().map(Json).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no job {id}")))
}

async fn repo_view(State(app): State<Arc<App>>, Path(name): Path<String>) -> Reply<View> {
    Ok(Json(app.repo(&name)?.view()))
}

async fn page() -> Html<&'static str> {
    Html(include_str!("page.html"))
}

async fn stats_view(State(app): State<Arc<App>>) -> Json<Value> {
    let mut repos = Vec::with_capacity(app.repos.len());
    let mut recent = Vec::new();
    for (name, repo) in &app.repos {
        let history = repo.recent(RECENT_PER_REPO).await;
        let last_build = history.iter().rev().find(|record| record["kind"] == "build").cloned();
        recent.extend(history.into_iter().filter(|record| record["kind"] != "build"));
        repos.push(json!({ "name": name, "view": repo.view(), "last_build": last_build }));
    }
    recent.sort_by_key(|record| Reverse(record["at"].as_u64()));
    recent.truncate(RECENT_JOBS);
    let mut counts = BTreeMap::from([("queued", 0_u32), ("running", 0), ("done", 0), ("failed", 0)]);
    for job in app.jobs.lock().unwrap_or_else(PoisonError::into_inner).values() {
        let state = match job {
            Job::Queued => "queued",
            Job::Running => "running",
            Job::Done { .. } => "done",
            Job::Failed { .. } => "failed",
        };
        *counts.entry(state).or_default() += 1;
    }
    let machine = app.machine.view(&app.state_dir).await;
    Json(json!({ "machine": machine, "repos": repos, "recent": recent, "jobs": counts }))
}
