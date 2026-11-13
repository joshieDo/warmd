use std::{collections::BTreeMap, path::Path};

use serde_json::{Value, json, to_vec_pretty};
use tokio::{
    fs::{create_dir_all, read_dir, read_to_string, write},
    process::Command,
};

use crate::{metrics::Phase, repo, stats::now_secs};

const KEPT: usize = 26;

pub(crate) struct Check<'a> {
    pub(crate) id: &'a str,
    pub(crate) branch: Option<&'a str>,
    pub(crate) base: &'a str,
    pub(crate) started_at: f64,
    pub(crate) phases: &'a [Phase],
    pub(crate) warmth: &'a Value,
}

pub(crate) async fn keep(state: &Path, from: &Path, check: &Check<'_>) -> anyhow::Result<()> {
    let root = state.join("slapslop-evidence");
    create_dir_all(&root).await?;
    let mut entries = read_dir(&root).await?;
    let mut count = 0;
    while entries.next_entry().await?.is_some() {
        count += 1;
    }
    if count >= KEPT {
        return Ok(());
    }
    let dir = root.join(check.id);
    create_dir_all(&dir).await?;
    for part in ["build.log", "perf", ".warmd-evidence"] {
        let source = from.join(part);
        if source.exists() {
            Command::new("cp").arg("-a").arg(&source).arg(dir.join(part.trim_start_matches('.'))).status().await?;
        }
    }
    let shell = dir.join("warmd-evidence");
    let read = async |name: &str| read_to_string(shell.join(name)).await.map(|text| text.trim().to_owned()).ok();
    let fast_exit_kinds = read_to_string(dir.join("perf/fast-exits.tsv"))
        .await
        .map(|text| text.lines().filter_map(|line| line.split('\t').nth(1).map(str::to_owned)).collect::<Vec<_>>())
        .ok();
    let meta = json!({
        "job_id": check.id,
        "branch": check.branch,
        "base": check.base,
        "slapslop": repo::installed_slapslop().await,
        "started_at": check.started_at,
        "phases": check.phases,
        "slapslop_rebuilt": check.warmth["rebuilt"]["slapslop"],
        "slapslop_rebuilt_from": read("rebuilt-method").await,
        "slapslop_cpu_secs": check.warmth["cpu_secs"]["slapslop"],
        "load_at_slapslop_start": read("load-start").await,
        "load_at_slapslop_end": read("load-end").await,
        "cpus": read("cpus").await,
        "slapslop_env": read("env").await,
        "fast_exit_kinds": fast_exit_kinds,
        "warmth": check.warmth,
    });
    write(dir.join("meta.json"), to_vec_pretty(&meta)?).await?;
    Ok(())
}

pub(crate) async fn keep_main(
    metrics_file: &Path,
    gen_dir: &Path,
    sha: &str,
    phases: &[Phase],
    jobs: &BTreeMap<String, Value>,
) {
    let Some(slapslop) = jobs.get("slapslop") else {
        return;
    };
    let id = format!("main-{}-{}", sha.get(..8).unwrap_or(sha), now_secs());
    let warmth = json!({
        "rebuilt": { "slapslop": slapslop["rebuilt"] },
        "cpu_secs": { "slapslop": slapslop["cpu_secs"] },
        "jobs": jobs,
    });
    let check = Check { id: &id, branch: None, base: sha, started_at: now_secs() as f64, phases, warmth: &warmth };
    let state = metrics_file.parent().unwrap_or_else(|| Path::new("."));
    if let Err(err) = keep(state, &gen_dir.join("target/slapslop"), &check).await {
        eprintln!("warmd: keeping slapslop evidence of main {sha}: {err:#}");
    }
}
