use std::{path::Path, time::Instant};

use serde::Serialize;
use serde_json::{Value, from_str, json, to_string};
use tokio::{
    fs::{OpenOptions, read_to_string, rename, write},
    io::AsyncWriteExt as _,
    sync::Mutex,
};

use crate::stats::now_secs;

pub(crate) const RETAIN_SECS: u64 = 4 * 24 * 3600;

static WRITE: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Phase {
    name: String,
    secs: f64,
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
        self.phases.push(Phase { name: name.into(), secs: now.duration_since(self.last).as_secs_f64() });
        self.last = now;
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
