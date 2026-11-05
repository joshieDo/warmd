use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::check::Request;

const REMOTE_TTL: Duration = Duration::from_secs(20);

const AFFINITY_WAIT: Duration = Duration::from_secs(10);

const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    id: Uuid,
    req: Request,
    since: Instant,
    attempt: u32,
    lapsed: Option<String>,
}

impl Pending {
    pub(crate) fn new(id: Uuid, req: Request) -> Self {
        Self { id, req, since: Instant::now(), attempt: 0, lapsed: None }
    }

    pub(crate) const fn id(&self) -> Uuid {
        self.id
    }

    pub(crate) const fn req(&self) -> &Request {
        &self.req
    }

    pub(crate) fn into_req(self) -> Request {
        self.req
    }

    fn key(&self) -> Option<(String, String)> {
        self.req.branch().map(|branch| (self.req.repo().to_owned(), branch.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    Local,
    Remote(String),
}

#[derive(Debug)]
pub(crate) struct Balancer {
    state: Mutex<State>,
    wake: Notify,
}

impl Balancer {
    pub(crate) fn new(remotes: Vec<String>) -> Self {
        Self {
            state: Mutex::new(State {
                pending: Vec::new(),
                remotes: remotes.into_iter().map(|addr| (addr, Remote::default())).collect(),
                local: HashMap::new(),
                affinity: HashMap::new(),
            }),
            wake: Notify::new(),
        }
    }

    pub(crate) const fn wake(&self) -> &Notify {
        &self.wake
    }

    pub(crate) fn enqueue(&self, job: Pending) {
        self.state().pending.push(job);
        self.wake.notify_waiters();
    }

    pub(crate) fn next(&self, local_free: &dyn Fn(&str, u32) -> bool) -> Option<(Pending, Source)> {
        let mut state = self.state();
        let (index, source) =
            state.pending.iter().enumerate().find_map(|(index, job)| Some((index, state.pick(job, local_free)?)))?;
        let mut job = state.pending.remove(index);
        job.attempt += 1;
        let repo = job.req.repo().to_owned();
        match &source {
            Source::Local => *state.local.entry(repo).or_default() += 1,
            Source::Remote(addr) => {
                if let Some(remote) = state.remotes.get_mut(addr) {
                    remote.running += 1;
                    if let Some(free) = remote.free.get_mut(&repo)
                        && *free > 0
                    {
                        *free -= 1;
                    }
                }
            }
        }
        if let Some(key) = job.key() {
            state.affinity.insert(key, source.clone());
        }
        Some((job, source))
    }

    pub(crate) fn ended(&self, repo: &str, source: &Source) {
        let mut state = self.state();
        match source {
            Source::Local => {
                if let Some(count) = state.local.get_mut(repo) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        state.local.remove(repo);
                    }
                }
            }
            Source::Remote(addr) => {
                if let Some(remote) = state.remotes.get_mut(addr) {
                    remote.running = remote.running.saturating_sub(1);
                }
            }
        }
        drop(state);
        self.wake.notify_waiters();
    }

    pub(crate) fn give_back(&self, mut job: Pending, addr: &str, why: &str) -> Option<String> {
        let mut state = self.state();
        if let Some(remote) = state.remotes.get_mut(addr) {
            remote.seen = None;
            remote.free.clear();
            remote.error = Some(why.to_owned());
        }
        state.affinity.retain(|_, owner| !matches!(owner, Source::Remote(held) if held == addr));
        if job.attempt >= MAX_ATTEMPTS {
            return Some(format!("given back {MAX_ATTEMPTS} times, the last by {addr}: {why}"));
        }
        job.lapsed = Some(addr.to_owned());
        job.since = Instant::now();
        state.pending.insert(0, job);
        drop(state);
        self.wake.notify_waiters();
        None
    }

    pub(crate) fn add(&self, addr: &str) -> bool {
        let added = self.state().remotes.insert(addr.to_owned(), Remote::default()).is_none();
        self.wake.notify_waiters();
        added
    }

    pub(crate) fn remove(&self, addr: &str) -> bool {
        let mut state = self.state();
        state.affinity.retain(|_, owner| !matches!(owner, Source::Remote(held) if held == addr));
        state.remotes.remove(addr).is_some()
    }

    pub(crate) fn remotes(&self) -> Vec<String> {
        self.state().remotes.keys().cloned().collect()
    }

    pub(crate) fn polled(&self, addr: &str, answer: anyhow::Result<HashMap<String, usize>>) {
        let mut state = self.state();
        let Some(remote) = state.remotes.get_mut(addr) else { return };
        match answer {
            Ok(free) => {
                remote.seen = Some(Instant::now());
                remote.free = free;
                remote.error = None;
            }
            Err(why) => remote.error = Some(format!("{why:#}")),
        }
        drop(state);
        self.wake.notify_waiters();
    }

    pub(crate) fn heard(&self, addr: &str) {
        if let Some(remote) = self.state().remotes.get_mut(addr) {
            remote.seen = Some(Instant::now());
        }
    }

    pub(crate) fn view(&self) -> Value {
        let state = self.state();
        let remotes = state
            .remotes
            .iter()
            .map(|(addr, remote)| {
                let mut free =
                    remote.free.iter().filter(|(_, free)| **free > 0).map(|(repo, _)| repo.clone()).collect::<Vec<_>>();
                free.sort();
                json!({
                    "addr": addr,
                    "up": remote.up(),
                    "seen_secs_ago": remote.seen.map(|at| at.elapsed().as_secs()),
                    "free_for": free,
                    "running": remote.running,
                    "error": remote.error,
                })
            })
            .collect::<Vec<_>>();
        json!({ "remotes": remotes, "pending": state.pending.len() })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Default)]
struct Remote {
    seen: Option<Instant>,
    free: HashMap<String, usize>,
    running: usize,
    error: Option<String>,
}

impl Remote {
    fn up(&self) -> bool {
        self.seen.is_some_and(|at| at.elapsed() < REMOTE_TTL)
    }
}

#[derive(Debug)]
struct State {
    pending: Vec<Pending>,
    remotes: BTreeMap<String, Remote>,
    local: HashMap<String, u32>,
    affinity: HashMap<(String, String), Source>,
}

impl State {
    fn up(&self, source: &Source) -> bool {
        match source {
            Source::Local => true,
            Source::Remote(addr) => self.remotes.get(addr).is_some_and(Remote::up),
        }
    }

    fn free(&self, source: &Source, repo: &str, local_free: &dyn Fn(&str, u32) -> bool) -> bool {
        match source {
            Source::Local => local_free(repo, self.local.get(repo).copied().unwrap_or_default()),
            Source::Remote(addr) => self
                .remotes
                .get(addr)
                .is_some_and(|remote| remote.up() && remote.free.get(repo).copied().unwrap_or_default() > 0),
        }
    }

    fn pick(&self, job: &Pending, local_free: &dyn Fn(&str, u32) -> bool) -> Option<Source> {
        let repo = job.req.repo();
        let waited = job.since.elapsed() >= AFFINITY_WAIT;
        if self.free(&Source::Local, repo, local_free) {
            return Some(Source::Local);
        }
        if !job.req.remote_ok() {
            return None;
        }
        if let Some(owner @ Source::Remote(_)) = job.key().and_then(|key| self.affinity.get(&key))
            && self.up(owner)
        {
            if self.free(owner, repo, local_free) {
                return Some(owner.clone());
            }
            if !waited {
                return None;
            }
        }
        self.remotes
            .iter()
            .filter(|(addr, _)| waited || job.lapsed.as_ref() != Some(*addr))
            .filter(|(_, remote)| remote.up() && remote.free.get(repo).copied().unwrap_or_default() > 0)
            .max_by_key(|(_, remote)| (remote.free.get(repo).copied().unwrap_or_default(), usize::MAX - remote.running))
            .map(|(addr, _)| Source::Remote(addr.clone()))
    }
}
