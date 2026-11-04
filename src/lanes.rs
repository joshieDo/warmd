//! A tree's jobs run in lanes: the jobs of one lane one after another (every clippy job shares `target/` and
//! cargo's build lock, so they would only queue), the lanes side by side (clippy, rustfmt, slapslop).

use std::time::Instant;

use crate::{
    config::Job,
    diag::Diag,
    metrics::Clock,
    sandbox::{LaneJob, Output, Sandbox},
};

/// Runs every planned job (with its command) by lane, calls `each` as each finishes, and marks each job's span
/// on `clock` in its lane.
pub(crate) async fn run<'job>(
    sandbox: Sandbox<'_>,
    planned: Vec<(&'job Job, String)>,
    clock: &mut Clock,
    mut each: impl FnMut(&'job Job, Output, Vec<Diag>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut names: Vec<&str> = Vec::new();
    let mut lanes: Vec<Vec<LaneJob<'job>>> = Vec::new();
    for (index, (job, cmd)) in planned.into_iter().enumerate() {
        let lane_job = LaneJob { index, job, cmd };
        if let Some(at) = names.iter().position(|known| *known == job.lane()) {
            lanes[at].push(lane_job);
        } else {
            names.push(job.lane());
            lanes.push(vec![lane_job]);
        }
    }
    let mut lane_free = vec![Instant::now(); lanes.len()];
    sandbox
        .diagnose_lanes(&lanes, |done, out, found| {
            let at = names.iter().position(|name| *name == done.job.lane()).unwrap_or_default();
            let now = Instant::now();
            clock.span(format!("job:{}", done.job.name), lane_free[at], now, vec![done.cmd.clone()], names[at]);
            lane_free[at] = now;
            each(done.job, out, found)
        })
        .await?;
    clock.catch_up();
    Ok(())
}
