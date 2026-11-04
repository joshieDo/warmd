use std::{cell::RefCell, path::Path, time::Instant};

use serde::Serialize;
use serde_json::{Value, from_str, json, to_string};
use tokio::{
    fs::{OpenOptions, read_to_string, rename, write},
    io::AsyncWriteExt as _,
    sync::Mutex,
    task_local,
};

use crate::stats::now_secs;

pub(crate) const RETAIN_SECS: u64 = 4 * 24 * 3600;

static WRITE: Mutex<()> = Mutex::const_new(());

const COMMAND_CHARS: usize = 600;

task_local! {
    static COMMANDS: RefCell<Vec<String>>;
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Phase {
    name: String,
    start_secs: f64,
    secs: f64,
    commands: Vec<String>,
    /// The lane of a job that ran beside others; none for a step of the run itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    lane: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Clock {
    started: Instant,
    last: Instant,
    phases: Vec<Phase>,
}

impl Clock {
    pub(crate) fn start() -> Self {
        let now = Instant::now();
        Self { started: now, last: now, phases: Vec::new() }
    }

    pub(crate) fn mark(&mut self, name: impl Into<String>) {
        let now = Instant::now();
        self.push(name.into(), self.last, now, take_commands(), None);
        self.last = now;
    }

    /// A phase that ran from `start` to `end` in `lane`, beside others.
    pub(crate) fn span(&mut self, name: String, start: Instant, end: Instant, commands: Vec<String>, lane: &str) {
        self.push(name, start, end, commands, Some(lane.to_owned()));
    }

    fn push(&mut self, name: String, start: Instant, end: Instant, commands: Vec<String>, lane: Option<String>) {
        self.phases.push(Phase {
            name,
            start_secs: start.duration_since(self.started).as_secs_f64(),
            secs: end.duration_since(start).as_secs_f64(),
            commands,
            lane,
        });
    }

    /// Starts the next marked phase now, after phases added with `span`.
    pub(crate) fn catch_up(&mut self) {
        self.last = Instant::now();
    }

    fn total_secs(&self) -> f64 {
        self.last.duration_since(self.started).as_secs_f64()
    }

    fn phases(&self) -> &[Phase] {
        &self.phases
    }

    pub(crate) fn into_phases(self) -> Vec<Phase> {
        self.phases
    }
}

pub(crate) async fn record(file: &Path, kind: &str, fields: Value, clock: &Clock) {
    let at = now_secs();
    let line =
        json!({ "at": at, "kind": kind, "fields": fields, "total_secs": clock.total_secs(), "phases": clock.phases() });
    let _write = WRITE.lock().await;
    if let Err(err) = async {
        let mut out = OpenOptions::new().create(true).append(true).open(file).await?;
        out.write_all(format!("{}\n", to_string(&line)?).as_bytes()).await?;
        prune(file, at).await
    }
    .await
    {
        eprintln!("warmd: writing {}: {err:#}", file.display());
    }
}

async fn prune(file: &Path, now: u64) -> anyhow::Result<()> {
    let text = read_to_string(file).await?;
    let fresh = |line: &str| {
        from_str::<Value>(line).ok().and_then(|record| record["at"].as_u64()).unwrap_or_default() + RETAIN_SECS >= now
    };
    if text.lines().next().is_none_or(fresh) {
        return Ok(());
    }
    let kept = text.lines().filter(|line| fresh(line)).map(|line| format!("{line}\n")).collect::<String>();
    let tmp = file.with_extension("jsonl.tmp");
    write(&tmp, kept).await?;
    rename(&tmp, file).await?;
    Ok(())
}

pub(crate) async fn with_commands<F: Future>(future: F) -> F::Output {
    COMMANDS.scope(RefCell::new(Vec::new()), future).await
}

/// The commands noted since the last take, in this task's scope.
fn take_commands() -> Vec<String> {
    COMMANDS.try_with(RefCell::take).unwrap_or_default()
}

pub(crate) fn note_command(cmd: &str) {
    let short = cmd.chars().take(COMMAND_CHARS).collect::<String>();
    // Commands outside a job (the main poll's ls-remote) have no timeline to join.
    if let Err(_outside) = COMMANDS.try_with(|commands| commands.borrow_mut().push(short)) {}
}
