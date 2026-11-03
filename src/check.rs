use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context as _, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{from_str, json};
use tokio::fs::create_dir_all;

use crate::{
    config::{Profile, Step},
    diag::{self, Diag},
    metrics::{self, Clock, Phase},
    repo::{self, Repo},
    sandbox::{self, Sandbox, Tree},
};

const SLAPSLOP_BASELINE: &str = ".slapslop/baseline.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    repo: String,
    base: Base,
    #[serde(default)]
    patch: String,
    #[serde(default)]
    profiles: Vec<String>,
    #[serde(default)]
    fix: bool,
    slapslop_baseline: Option<String>,
    #[serde(skip)]
    baseline_only: bool,
}

impl Request {
    pub(crate) fn repo(&self) -> &str {
        &self.repo
    }

    pub(crate) fn baseline_only(self) -> Self {
        Self { baseline_only: true, ..self }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    ok: bool,
    ran_on: String,
    rebased_from: Option<String>,
    main_moved: bool,
    jobs: Vec<JobResult>,
    profiles: BTreeMap<String, Vec<Diag>>,
    fix_patch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Checked(Report),
    RebaseNeeded { files: Vec<String> },
    SlapslopBaseline { baseline: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Base {
    #[serde(rename = "ref")]
    branch: Option<String>,
    sha: String,
}

#[derive(Debug, Clone, Serialize)]
struct JobResult {
    name: String,
    exit: Option<i32>,
    new: Vec<Diag>,
    output: Option<String>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: String,
}

pub(crate) async fn run(
    repo: &Repo,
    profiles: &BTreeMap<String, Profile>,
    req: Request,
    layer: &Path,
    running: impl FnOnce(),
) -> (anyhow::Result<Outcome>, Vec<Phase>) {
    let mut clock = Clock::start();
    let kind = if req.baseline_only { "slapslop_baseline" } else { "check" };
    let outcome = check(repo, profiles, req, layer, running, &mut clock).await;
    if let Err(err) = repo::remove_tree(layer).await {
        eprintln!("warmd: {}: {err:#}", repo.name());
    }
    clock.mark("cleanup");
    let result = match &outcome {
        Ok(Outcome::Checked(report)) => (if report.ok { "ok" } else { "found" }).to_owned(),
        Ok(Outcome::RebaseNeeded { .. }) => "rebase_needed".to_owned(),
        Ok(Outcome::SlapslopBaseline { .. }) => "baseline".to_owned(),
        Err(err) => format!("failed: {err:#}"),
    };
    metrics::record(&repo.metrics_file(), kind, json!({ "repo": repo.name(), "result": result }), &clock).await;
    (outcome, clock.into_phases())
}

async fn check(
    repo: &Repo,
    profiles: &BTreeMap<String, Profile>,
    req: Request,
    layer: &Path,
    running: impl FnOnce(),
    clock: &mut Clock,
) -> anyhow::Result<Outcome> {
    let branch = &repo.cfg().branch;
    if let Some(asked) = &req.base.branch {
        ensure!(asked == branch, "{asked} is not tracked for {}, only {branch}", repo.name());
    }
    let extra = req
        .profiles
        .iter()
        .map(|name| {
            let profile = profiles.get(name).filter(|profile| profile.applies_to(repo.name()));
            Ok((name, profile.with_context(|| format!("no profile {name} for {}", repo.name()))?))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let current = repo.current().with_context(|| format!("{} has no generation yet", repo.name()))?;
    let (generation, rebased_from) = match repo.find(&req.base.sha) {
        Some(generation) => (generation, None),
        None => (current.clone(), Some(req.base.sha.clone())),
    };
    let main_moved = generation.sha() != current.sha();
    drop(current);
    let _slot = repo.slot().await?;
    running();
    clock.mark("queued");
    let (upper, work) = (layer.join("upper"), layer.join("work"));
    create_dir_all(&upper).await?;
    create_dir_all(&work).await?;
    clock.mark("setup");
    let sandbox =
        Sandbox { repo: repo.name(), tree: Tree::Overlay { lower: generation.dir(), upper: &upper, work: &work } };

    let apply = if rebased_from.is_some() { "git apply --3way -" } else { "git apply --index -" };
    let applied = if req.patch.trim().is_empty() { None } else { Some(sandbox.run(apply, Some(&req.patch)).await?) };
    if let Some(applied) = applied.filter(|applied| !applied.ok()) {
        if rebased_from.is_some() {
            let conflicted = sandbox.run("git diff --name-only --diff-filter=U", None).await?;
            return Ok(Outcome::RebaseNeeded { files: conflicted.stdout.lines().map(str::to_owned).collect() });
        }
        bail!("the patch does not apply to {}: {}", generation.sha(), applied.stderr.trim());
    }
    clock.mark("apply");
    if let Some(baseline) = &req.slapslop_baseline {
        let written = sandbox.run(&format!("mkdir -p .slapslop && cat > {SLAPSLOP_BASELINE}"), Some(baseline)).await?;
        ensure!(written.ok(), "writing the slapslop baseline: {}", written.stderr.trim());
    }
    let commit = "git add -A && git -c user.name=warmd -c user.email=warmd@localhost commit -q --no-verify --allow-empty -m patch";
    let committed = sandbox.run(commit, None).await?;
    ensure!(committed.ok(), "committing the patch: {}", committed.stderr.trim());
    clock.mark("commit");
    if req.baseline_only {
        let update = "cargo-slapslop baseline update \"$PWD\" --accept-new --reason warmd >&2 && cat ";
        let out = sandbox.run(&format!("{update}{SLAPSLOP_BASELINE}"), None).await?;
        ensure!(out.ok(), "generating the slapslop baseline: {}", out.tail());
        clock.mark("slapslop_baseline");
        return Ok(Outcome::SlapslopBaseline { baseline: out.stdout });
    }

    let all_jobs = repo.cfg().all_jobs();
    let mut jobs = Vec::with_capacity(all_jobs.len());
    for job in &all_jobs {
        let (out, found) = sandbox.diagnose(job).await?;
        let output = (!out.ok() && found.is_empty()).then(|| out.tail());
        let new = diag::new_only(found, generation.baseline(&job.name));
        jobs.push(JobResult { name: job.name.clone(), exit: out.code, new, output });
        clock.mark(format!("job:{}", job.name));
    }

    let added = diag::added_lines(&req.patch);
    let mut hits = BTreeMap::new();
    for (name, profile) in extra {
        let mut found = Vec::new();
        for step in &profile.steps {
            found.extend(run_step(sandbox, step, repo, &added).await?);
        }
        found.retain(|hit| added.get(&hit.file).is_some_and(|lines| lines.contains(&hit.line)));
        hits.insert(name.clone(), found);
        clock.mark(format!("profile:{name}"));
    }

    let fix_patch = if req.fix {
        for fix in all_jobs.iter().filter_map(|job| job.fix.as_deref()) {
            sandbox.run(fix, None).await?;
        }
        let diff = sandbox.run("git diff", None).await?.stdout;
        clock.mark("fix");
        (!diff.is_empty()).then_some(diff)
    } else {
        None
    };
    let ok = jobs.iter().all(|job| job.new.is_empty() && job.output.is_none()) && hits.values().all(Vec::is_empty);
    let ran_on = generation.sha().to_owned();
    let report = Report { ok, ran_on, rebased_from, main_moved, jobs, profiles: hits, fix_patch };
    Ok(Outcome::Checked(report))
}

async fn run_step(
    sandbox: Sandbox<'_>,
    step: &Step,
    repo: &Repo,
    added: &BTreeMap<String, BTreeSet<u32>>,
) -> anyhow::Result<Vec<Diag>> {
    match step {
        Step::Clippy { targets, args, conf_dir } => {
            let metadata = sandbox.run("cargo metadata --no-deps --format-version 1 --offline", None).await?;
            let crates = touched_crates(&metadata.stdout, &sandbox::work_path(repo.name()), added.keys())?;
            if crates.is_empty() {
                return Ok(Vec::new());
            }
            let packages = crates.iter().map(|krate| format!("-p {krate}")).collect::<Vec<_>>().join(" ");
            let conf = conf_dir.as_ref().map(|dir| format!("CLIPPY_CONF_DIR='{}' ", dir.display())).unwrap_or_default();
            let cmd = format!("{conf}{} {targets} {packages} -- {}", repo.cfg().clippy, args.join(" "));
            let out = sandbox.run(&cmd, None).await?;
            Ok(diag::parse(&out.stdout))
        }
        Step::Slapslop { args } => {
            let cmd = format!("cargo-slapslop run \"$PWD\" --diff HEAD~1 {}", args.join(" "));
            Ok(sandbox.slapslop(&cmd).await?.1)
        }
    }
}

fn touched_crates<'a>(
    metadata: &str,
    root: &str,
    files: impl Iterator<Item = &'a String>,
) -> anyhow::Result<BTreeSet<String>> {
    let metadata: Metadata = from_str(metadata).context("reading cargo metadata")?;
    let dirs = metadata
        .packages
        .iter()
        .filter_map(|package| {
            let manifest = package.manifest_path.strip_prefix(root)?.strip_prefix('/')?;
            Some((manifest.strip_suffix("Cargo.toml")?, package.name.as_str()))
        })
        .collect::<Vec<_>>();
    let owner = |file: &str| {
        dirs.iter().filter(|(dir, _)| file.starts_with(dir)).max_by_key(|(dir, _)| dir.len()).map(|(_, name)| *name)
    };
    Ok(files.filter_map(|file| owner(file)).map(str::to_owned).collect())
}
