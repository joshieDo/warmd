use std::{
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::{Value, json, to_string};
use tokio::{fs::OpenOptions, io::AsyncWriteExt as _};

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
    let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let line =
        json!({ "at": at, "kind": kind, "fields": fields, "total_secs": clock.total_secs(), "phases": clock.phases() });
    let written = async {
        let mut out = OpenOptions::new().create(true).append(true).open(file).await?;
        out.write_all(format!("{}\n", to_string(&line)?).as_bytes()).await?;
        anyhow::Ok(())
    };
    if let Err(err) = written.await {
        eprintln!("warmd: writing {}: {err:#}", file.display());
    }
}
