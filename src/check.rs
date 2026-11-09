use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, from_str, json};
use tokio::{
    fs::{copy, create_dir_all, read_dir, read_to_string, remove_dir_all, symlink_metadata, write},
    task::spawn_blocking,
};
use uuid::Uuid;

use crate::{
    config::{ArgsOverride, Binary, FULL_CLIPPY, Profile, Step, Tests},
    diag::{self, Diag},
    lanes,
    metrics::{self, Clock, Phase},
    repo::{self, Repo},
    sandbox::{self, Sandbox, Tree},
    stats::now_secs,
};

const SLAPSLOP_BASELINE: &str = ".slapslop/baseline.toml";

/// How long a built binary stays for download.
const BINARY_KEPT: Duration = Duration::from_hours(1);

const COMMIT: &str = "git -c user.name=warmd -c user.email=warmd@localhost commit -q --no-verify --allow-empty";

const SYNC_TREE: &str = r#"set -eu
index=$(mktemp)
GIT_INDEX_FILE=$index git read-tree {gen}
if {apply}; then GIT_INDEX_FILE=$index git apply --cached -; fi
tree=$(GIT_INDEX_FILE=$index git write-tree)
rm -f "$index"
{ git diff --name-only --no-renames HEAD "$tree"; git diff --name-only HEAD; git ls-files --others --exclude-standard; } | sort -u | while IFS= read -r path; do
  if git cat-file -e "$tree:$path" 2>/dev/null; then git checkout -q "$tree" -- "$path"; else rm -f -- "$path"; git rm -q --cached --ignore-unmatch -- "$path"; fi
done
commit=$(git -c user.name=warmd -c user.email=warmd@localhost commit-tree "$tree" -p {gen} -m patch)
git reset -q --soft "$commit"
git read-tree "$commit"
"#;

const FULL_NOTICE: &str = "Advisory: the full set (slapslop --all-lints and slapslop's recommended clippy set) on your added lines. Fix what you reasonably can; hazy hits and deliberate patterns may stay. Only the CI jobs (ok / ok_precise) decide whether the change can merge.";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    repo: String,
    base: Base,
    #[serde(default)]
    patch: String,
    #[serde(default)]
    full: bool,
    #[serde(default)]
    fix: bool,
    slapslop_baseline: Option<String>,
    branch: Option<String>,
    #[serde(default)]
    args: BTreeMap<String, ArgsOverride>,
    session: Option<String>,
    #[serde(default)]
    skip: Vec<String>,
    #[serde(default)]
    only: Vec<String>,
    tests: Option<Tests>,
    binary: Option<Binary>,
    #[serde(skip)]
    baseline_only: bool,
    #[serde(skip)]
    full_only: bool,
}

impl Request {
    pub(crate) fn repo(&self) -> &str {
        &self.repo
    }

    pub(crate) const fn kind(&self) -> &'static str {
        if self.baseline_only {
            "slapslop_baseline"
        } else if self.binary.is_some() {
            "binary"
        } else if self.tests.is_some() {
            "test"
        } else if self.full_only {
            "full"
        } else {
            "check"
        }
    }

    pub(crate) fn summary(&self) -> Value {
        json!({
            "session": self.session,
            "full": self.full,
            "full_only": self.full_only,
            "only": self.only,
            "skip": self.skip,
            "args": self.args,
            "fix": self.fix,
            "patch_lines": self.patch.lines().count(),
            "patch_files": changed_files(&self.patch).len(),
            "slapslop_baseline": self.slapslop_baseline.is_some(),
            "tests": self.tests,
            "binary": self.binary,
        })
    }

    pub(crate) const fn tests(&self) -> Option<&Tests> {
        self.tests.as_ref()
    }

    pub(crate) const fn binary(&self) -> Option<&Binary> {
        self.binary.as_ref()
    }

    pub(crate) fn alone(self) -> Self {
        Self { full: false, fix: false, ..self }
    }

    pub(crate) fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub(crate) fn patch(&self) -> &str {
        &self.patch
    }

    pub(crate) fn base_sha(&self) -> &str {
        &self.base.sha
    }

    pub(crate) const fn wants_full(&self) -> bool {
        self.full
    }

    pub(crate) fn split_full(self) -> (Self, Self) {
        let full = Self { full_only: true, ..self.clone() };
        (Self { full: false, ..self }, full)
    }

    pub(crate) fn baseline_only(self) -> Self {
        Self { baseline_only: true, ..self }
    }

    pub(crate) const fn remote_ok(&self) -> bool {
        !self.full_only && self.binary.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    ok: bool,
    ok_precise: bool,
    session_reused: bool,
    ran_on: String,
    rebased_from: Option<String>,
    main_moved: bool,
    jobs: Vec<JobResult>,
    not_run: Vec<String>,
    /// Jobs skipped because they fail on main itself: nothing a patch can fix, so they don't decide `ok`.
    broken_on_main: Vec<String>,
    full: Option<FullReport>,
    fix_patch: Option<String>,
}

impl Report {
    /// The cheap facts a dashboard needs to tell a warm check from a cold one.
    fn metrics(&self) -> Value {
        json!({
            "session_reused": self.session_reused,
            "rebased": self.rebased_from.is_some(),
            "main_moved": self.main_moved,
            "rebuilt": self.jobs.iter().filter_map(|job| Some((job.name.clone(), job.rebuilt?))).collect::<BTreeMap<_, _>>(),
            "new": self.jobs.iter().map(|job| (job.name.clone(), job.new.len())).collect::<BTreeMap<_, _>>(),
            "cpu_secs": self.jobs.iter().filter_map(|job| Some((job.name.clone(), job.cpu_secs?))).collect::<BTreeMap<_, _>>(),
        })
    }

    fn summary(&self) -> String {
        let full = self
            .full
            .as_ref()
            .filter(|full| !full.hits.is_empty())
            .map_or_else(String::new, |full| format!(" (full: {} advisory)", full.hits.len()));
        let full = if self.broken_on_main.is_empty() {
            full
        } else {
            format!("{full} ({} broken on main, skipped)", self.broken_on_main.join(", "))
        };
        if self.ok && !self.not_run.is_empty() {
            return format!("ok (partial: {} not run){full}", self.not_run.join(", "));
        }
        if self.ok {
            return format!("ok{full}");
        }
        let jobs = self.jobs.iter().filter_map(|job| match (job.new.len(), &job.output) {
            (0, Some(_)) => Some(format!("{} interrupted", job.name)),
            (0, None) => None,
            (count, _) => Some(format!("{} {count}", job.name)),
        });
        format!("found: {}{full}", jobs.collect::<Vec<_>>().join(", "))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Checked(Report),
    RebaseNeeded { files: Vec<String> },
    SlapslopBaseline { baseline: String },
    Built(Built),
}

impl Outcome {
    fn ran_on(&self) -> Option<String> {
        match self {
            Self::Checked(report) => Some(report.ran_on.clone()),
            Self::Built(built) => Some(built.ran_on.clone()),
            Self::RebaseNeeded { .. } | Self::SlapslopBaseline { .. } => None,
        }
    }

    pub(crate) fn binary_file(&self) -> Option<&Path> {
        match self {
            Self::Built(Built { binary: Some(binary), .. }) => Some(&binary.file),
            Self::Built(_) | Self::Checked(_) | Self::RebaseNeeded { .. } | Self::SlapslopBaseline { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Built {
    ok: bool,
    ran_on: String,
    rebased_from: Option<String>,
    session_reused: bool,
    binary: Option<BuiltBinary>,
    output: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct BuiltBinary {
    name: String,
    profile: &'static str,
    bytes: u64,
    #[serde(skip)]
    file: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct FullReport {
    notice: &'static str,
    hits: Vec<Diag>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    custom_args: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    test: bool,
    rebuilt: Option<u32>,
    cpu_secs: Option<f64>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: String,
    dependencies: Vec<Dependency>,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    kind: Vec<String>,
    src_path: String,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
}

#[derive(Debug, Default)]
struct Scope {
    touched: BTreeSet<String>,
    affected: BTreeSet<String>,
    whole: bool,
    bins_touched: bool,
    benches_touched: bool,
}

impl Scope {
    fn of(metadata: &str, root: &str, files: &BTreeSet<String>) -> anyhow::Result<Self> {
        let metadata: Metadata = from_str(metadata).context("reading cargo metadata")?;
        let owner = |file: &str| {
            (metadata
                .packages
                .iter()
                .filter_map(|package| {
                    Some((
                        package.manifest_path.strip_prefix(root)?.strip_prefix('/')?.strip_suffix("Cargo.toml")?,
                        package.name.as_str(),
                    ))
                })
                .collect::<Vec<_>>())
            .iter()
            .filter(|(dir, _)| file.starts_with(dir))
            .max_by_key(|(dir, _)| dir.len())
            .map(|(_, name)| *name)
        };
        let mut scope = Self::default();
        for file in files {
            match owner(file) {
                Some(name) => {
                    scope.touched.insert(name.to_owned());
                }
                None => scope.whole = true,
            }
        }
        let bin_roots = metadata
            .packages
            .iter()
            .flat_map(|package| &package.targets)
            .filter(|target| target.kind.iter().any(|kind| kind == "bin"))
            .filter_map(|target| target.src_path.strip_prefix(root)?.strip_prefix('/'))
            .collect::<Vec<_>>();
        scope.bins_touched = files
            .iter()
            .any(|file| bin_roots.contains(&file.as_str()) || file.starts_with("bin/") || file.contains("/bin/"));
        scope.benches_touched = files.iter().any(|file| {
            ["benches/", "examples/"].iter().any(|dir| file.starts_with(dir) || file.contains(&format!("/{dir}")))
                || metadata.packages.iter().flat_map(|package| &package.targets).any(|target| {
                    target.kind.iter().any(|kind| kind == "bench" || kind == "example")
                        && target.src_path.strip_prefix(root).and_then(|path| path.strip_prefix('/'))
                            == Some(file.as_str())
                })
        });
        scope.affected.clone_from(&scope.touched);
        loop {
            let dependents = metadata
                .packages
                .iter()
                .filter(|package| !scope.affected.contains(&package.name))
                .filter(|package| package.dependencies.iter().any(|dep| scope.affected.contains(&dep.name)))
                .map(|package| package.name.clone())
                .collect::<Vec<_>>();
            if dependents.is_empty() {
                return Ok(scope);
            }
            scope.affected.extend(dependents);
        }
    }

    fn flags(names: &BTreeSet<String>) -> String {
        names.iter().map(|name| format!("-p {name}")).collect::<Vec<_>>().join(" ")
    }

    fn command(&self, scoped: &str) -> Option<String> {
        (!self.affected.is_empty()).then(|| {
            let list = self.affected.iter().map(String::as_str).collect::<Vec<_>>().join(",");
            scoped
                .replace("{touched}", &Self::flags(&self.touched))
                .replace("{packages}", &Self::flags(&self.affected))
                .replace("{packages_csv}", &list)
        })
    }
}

pub(crate) async fn run(
    repo: &Repo,
    full: &Profile,
    req: Request,
    layer: &Path,
    running: impl FnOnce(),
    progress: impl Fn(Value),
) -> (anyhow::Result<Outcome>, Vec<Phase>) {
    let mut clock = Clock::start();
    let load_at_start = read_to_string("/proc/loadavg")
        .await
        .ok()
        .and_then(|text| text.split_whitespace().next().and_then(|load| load.parse::<f64>().ok()));
    let kind = req.kind();
    let (base, branch, dirty) = (req.base.sha.clone(), req.branch.clone(), !req.patch.trim().is_empty());
    let request = req.summary();
    let session_name = req.session.clone().filter(|_| !req.full_only);
    let (layer, session_guard) = match session_name.clone() {
        Some(name) => {
            let (dir, guard) = repo.session(&name).await;
            clock.mark("wait_branch");
            (dir, Some(guard))
        }
        None => (layer.to_owned(), None),
    };
    let outcome = check(repo, full, req, &layer, running, progress, &mut clock).await;
    match spawn_blocking({
        let from = layer.clone();
        move || sandbox::harvest_rustc_cache(&from)
    })
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => eprintln!("warmd: {}: keeping the rustc cache's new entries: {err:#}", repo.name()),
        Err(err) => eprintln!("warmd: {}: keeping the rustc cache's new entries: {err}", repo.name()),
    }
    let layer_bytes = repo::dir_bytes(&layer.join("upper")).await.ok();
    if session_guard.is_some() && matches!(&outcome, Ok(Outcome::Checked(_) | Outcome::Built(_))) {
        let built_on = outcome.as_ref().ok().and_then(Outcome::ran_on);
        let meta = json!({ "gen": built_on, "used_at": now_secs(), "session": session_name, "bytes": layer_bytes })
            .to_string();
        if let Err(err) = write(layer.join("meta.json"), meta).await {
            eprintln!("warmd: {}: {err:#}", repo.name());
        }
    } else if let Err(err) = repo::remove_tree(&layer).await {
        eprintln!("warmd: {}: {err:#}", repo.name());
    }
    clock.mark("cleanup");
    let result = match &outcome {
        Ok(Outcome::Checked(report)) => report.summary(),
        Ok(Outcome::RebaseNeeded { .. }) => "rebase_needed".to_owned(),
        Ok(Outcome::SlapslopBaseline { .. }) => "baseline".to_owned(),
        Ok(Outcome::Built(built)) => if built.ok { "built" } else { "build failed" }.to_owned(),
        Err(err) => format!("failed: {err:#}"),
    };
    let ran_on = outcome.as_ref().ok().and_then(Outcome::ran_on);
    let warmth = outcome.as_ref().ok().and_then(|done| match done {
        Outcome::Checked(report) => Some(report.metrics()),
        Outcome::RebaseNeeded { .. } | Outcome::SlapslopBaseline { .. } | Outcome::Built(_) => None,
    });
    let fields = json!({
        "warmth": warmth,
        "load_at_start": load_at_start,
        "repo": repo.name(),
        "result": result,
        "base": base,
        "dirty": dirty,
        "branch": branch,
        "ran_on": ran_on,
        "layer_bytes": layer_bytes,
        "request": request,
    });
    metrics::record(&repo.metrics_file(), kind, fields, &clock).await;
    (outcome, clock.into_phases())
}

async fn check(
    repo: &Repo,
    full: &Profile,
    req: Request,
    layer: &Path,
    running: impl FnOnce(),
    progress: impl Fn(Value),
    clock: &mut Clock,
) -> anyhow::Result<Outcome> {
    let cfg = repo.cfg();
    let branch = &cfg.branch;
    if let Some(asked) = &req.base.branch {
        ensure!(asked == branch, "{asked} is not tracked for {}, only {branch}", repo.name());
    }
    if let Some(key) = req
        .args
        .keys()
        .chain(&req.skip)
        .chain(&req.only)
        .find(|key| !cfg.jobs.iter().any(|job| job.answers_to(key, &cfg.jobs)))
    {
        bail!("no job of {} answers to {key}", repo.name());
    }
    if let Err(err) = repo.catch_up_to(&req.base.sha).await {
        eprintln!("warmd: {}: building main {} for a check: {err:#}", repo.name(), req.base.sha);
    }
    clock.mark("wait_main");
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
    let session_reused =
        (read_to_string(layer.join("meta.json")).await.ok().and_then(|text| from_str::<Value>(&text).ok()))
            .is_some_and(|meta| meta["gen"] == generation.sha());
    if !session_reused {
        repo::remove_tree(layer).await?;
    }
    for dir in ["upper", "work"] {
        create_dir_all(layer.join(dir)).await?;
    }
    clock.mark("setup");
    let cache = repo.cache_dir();
    let sandbox = Sandbox { repo: repo.name(), tree: Tree::Overlay { lower: generation.dir(), layer, cache: &cache } };

    if rebased_from.is_some() {
        if session_reused {
            let reset = sandbox
                .run_described(
                    &format!("git reset --hard {}  # drop the session's last patch", &generation.sha()[..7]),
                    &format!("git reset -q --hard {}", generation.sha()),
                    None,
                )
                .await?;
            ensure!(reset.ok(), "resetting the session: {}", reset.stderr.trim());
        }
        if if req.patch.trim().is_empty() {
            None
        } else {
            Some(sandbox.run("git apply --3way -", Some(&req.patch)).await?)
        }
        .is_some_and(|applied| !applied.ok())
        {
            return Ok(Outcome::RebaseNeeded {
                files: sandbox
                    .run("git diff --name-only --diff-filter=U", None)
                    .await?
                    .stdout
                    .lines()
                    .map(str::to_owned)
                    .collect(),
            });
        }
        let committed = sandbox.run(&format!("git add -A && {COMMIT} -m patch"), None).await?;
        ensure!(committed.ok(), "committing the patch: {}", committed.stderr.trim());
    } else {
        let synced = sandbox
            .run_described(
                &format!("git apply --cached  # patch onto {}, rewriting only changed files", &generation.sha()[..7]),
                &(SYNC_TREE
                    .replace("{gen}", generation.sha())
                    .replace("{apply}", if req.patch.trim().is_empty() { "false" } else { "true" })),
                Some(&req.patch),
            )
            .await?;
        ensure!(synced.ok(), "the patch does not apply to {}: {}", generation.sha(), synced.stderr.trim());
    }
    clock.mark("apply");
    if let Some(baseline) = &req.slapslop_baseline {
        let written = sandbox.run(&format!("mkdir -p .slapslop && cat > {SLAPSLOP_BASELINE}"), Some(baseline)).await?;
        ensure!(written.ok(), "writing the slapslop baseline: {}", written.stderr.trim());
        let committed = sandbox.run(&format!("git add -A && {COMMIT} --amend --no-edit"), None).await?;
        ensure!(committed.ok(), "committing the baseline: {}", committed.stderr.trim());
    }
    clock.mark("commit");
    if req.baseline_only {
        let update = "cargo-slapslop baseline update \"$PWD\" --accept-new --reason warmd >&2 && cat ";
        let out = sandbox.run(&format!("{update}{SLAPSLOP_BASELINE}"), None).await?;
        ensure!(out.ok(), "generating the slapslop baseline: {}", out.tail());
        clock.mark("slapslop_baseline");
        return Ok(Outcome::SlapslopBaseline { baseline: out.stdout });
    }
    if let Some(binary) = &req.binary {
        let out = sandbox.run(&binary.cmd(&cfg.build), None).await?;
        clock.mark(format!("build:{}", binary.profile()));
        let made =
            if out.ok() { Some(keep_binary(repo, binary, &out.stdout, layer, generation.dir()).await?) } else { None };
        clock.mark("copy_binary");
        return Ok(Outcome::Built(Built {
            ok: made.is_some(),
            ran_on: generation.sha().to_owned(),
            rebased_from,
            session_reused,
            binary: made,
            output: (!out.ok()).then(|| out.tail()),
        }));
    }

    let added = diag::added_lines(&req.patch);
    let scope = if req.full || cfg.jobs.iter().any(|job| job.on_patch.is_some() || job.bins_only || job.benches_only) {
        let scope = Scope::of(
            &sandbox.run("cargo metadata --no-deps --format-version 1 --offline", None).await?.stdout,
            &sandbox::work_path(repo.name()),
            &changed_files(&req.patch),
        )?;
        clock.mark("metadata");
        scope
    } else {
        Scope::default()
    };
    let packages = Scope::flags(&scope.touched);

    let all_jobs = &cfg.jobs;
    let not_run = if req.full_only {
        Vec::new()
    } else {
        all_jobs
            .iter()
            .filter(|job| req.tests.is_none() && !job.test && !job.wanted(false, &req.only, &req.skip, all_jobs))
            .map(|job| job.name.clone())
            .collect()
    };
    let mut planned = Vec::with_capacity(all_jobs.len());
    for job in
        all_jobs.iter().filter(|job| !req.full_only && job.wanted(req.tests.is_some(), &req.only, &req.skip, all_jobs))
    {
        if (job.bins_only && !scope.whole && !scope.bins_touched)
            || (job.benches_only && !scope.whole && !scope.benches_touched)
            || (job.test && !job.runs_named_tests())
            || (!job.test && generation.broken().contains_key(&job.name))
        {
            continue;
        }
        let template = match &job.on_patch {
            Some(scoped) if !scope.whole => match scope.command(scoped) {
                Some(cmd) => cmd,
                None => continue,
            },
            _ => job.cmd.clone(),
        };
        planned.push((job, template));
    }
    let order = planned.iter().map(|(job, _)| job.name.clone()).collect::<Vec<_>>();
    let mut pending = order.clone();
    let mut jobs = Vec::with_capacity(planned.len());
    progress(json!({ "partial": jobs, "pending": pending }));
    lanes::run(
        sandbox,
        planned
            .into_iter()
            .map(|(job, template)| {
                let custom = req
                    .args
                    .get(&job.name)
                    .or_else(|| req.args.iter().find(|(key, _)| job.answers_to(key, all_jobs)).map(|(_, over)| over));
                let cmd = job.render(&template, custom);
                match req.tests.as_ref() {
                    Some(tests) if job.test => (job, tests.fill(&cmd)),
                    Some(_) | None => (job, cmd),
                }
            })
            .collect(),
        clock,
        |job, out, mut found| {
            let custom = req
                .args
                .get(&job.name)
                .or_else(|| req.args.iter().find(|(key, _)| job.answers_to(key, all_jobs)).map(|(_, over)| over));
            let output = if job.test { Some(out.both()) } else { (!out.ok() && found.is_empty()).then(|| out.tail()) };
            let new = if custom.is_some() {
                found.retain(|hit| added.get(&hit.file).is_some_and(|lines| lines.contains(&hit.line)));
                found
            } else {
                diag::new_only(found, generation.baseline(&job.name))
            };
            jobs.push(JobResult {
                name: job.name.clone(),
                exit: out.code,
                new,
                output,
                custom_args: custom.is_some(),
                test: job.test,
                rebuilt: diag::rebuilt(&out.stdout),
                cpu_secs: out.cpu_secs,
            });
            pending.retain(|name| *name != job.name);
            progress(json!({ "partial": jobs, "pending": pending }));
            Ok(())
        },
    )
    .await?;
    // The report lists jobs as configured; progress listed them as they finished.
    jobs.sort_by_key(|job| order.iter().position(|name| *name == job.name));

    let full = if req.full {
        let mut hits = Vec::new();
        for step in &full.steps {
            hits.extend(run_step(sandbox, step, repo, &packages).await?);
            clock.mark(step.label());
        }
        hits.retain(|hit| added.get(&hit.file).is_some_and(|lines| lines.contains(&hit.line)));
        Some(FullReport { notice: FULL_NOTICE, hits })
    } else {
        None
    };

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
    let ok = jobs.iter().filter(|job| !job.test).all(|job| job.new.is_empty() && job.output.is_none());
    let ran_on = generation.sha().to_owned();
    Ok(Outcome::Checked(Report {
        ok,
        ok_precise: jobs
            .iter()
            .filter(|job| !job.test)
            .all(|job| job.new.iter().all(|hit| hit.hazy) && job.output.is_none()),
        session_reused,
        ran_on,
        rebased_from,
        main_moved,
        jobs,
        not_run,
        broken_on_main: generation.broken().keys().cloned().collect(),
        full,
        fix_patch,
    }))
}

async fn keep_binary(
    repo: &Repo,
    binary: &Binary,
    messages: &str,
    layer: &Path,
    lower: &Path,
) -> anyhow::Result<BuiltBinary> {
    let mut made = messages
        .lines()
        .filter_map(|line| from_str::<Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| {
            message["target"]["kind"].as_array().is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .filter_map(|message| {
            Some((message["target"]["name"].as_str()?.to_owned(), message["executable"].as_str()?.to_owned()))
        })
        .filter(|(name, _)| binary.bin().is_none_or(|bin| bin == name))
        .collect::<Vec<_>>();
    made.dedup();
    let (name, executable) = match made.as_slice() {
        [one] => one.clone(),
        [] => bail!("the build made no binary"),
        several => bail!(
            "the package has several binaries, name one with --bin: {}",
            several.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ")
        ),
    };
    let relative = Path::new(&executable)
        .strip_prefix(sandbox::work_path(repo.name()))
        .with_context(|| format!("{executable} is outside the tree"))?;
    let mut source = None;
    for candidate in [layer.join("upper").join(relative), lower.join(relative)] {
        if symlink_metadata(&candidate).await.is_ok_and(|meta| meta.is_file()) {
            source = Some(candidate);
            break;
        }
    }
    let source = source.with_context(|| format!("{executable} is in neither the tree's layer nor main's build"))?;
    let builds = repo.jobs_dir().with_file_name("builds");
    drop_old_binaries(&builds).await?;
    let dir = builds.join(Uuid::new_v4().to_string());
    create_dir_all(&dir).await?;
    let file = dir.join(&name);
    let bytes = copy(&source, &file).await.with_context(|| format!("copying {}", source.display()))?;
    Ok(BuiltBinary { name, profile: binary.profile(), bytes, file })
}

async fn drop_old_binaries(builds: &Path) -> anyhow::Result<()> {
    let mut entries = match read_dir(builds).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", builds.display())),
    };
    while let Some(entry) = entries.next_entry().await? {
        let modified = entry.metadata().await?.modified()?;
        if SystemTime::now().duration_since(modified).is_ok_and(|age| age > BINARY_KEPT) {
            remove_dir_all(entry.path()).await?;
        }
    }
    Ok(())
}

async fn run_step(sandbox: Sandbox<'_>, step: &Step, repo: &Repo, packages: &str) -> anyhow::Result<Vec<Diag>> {
    match step {
        Step::Clippy { targets, args, recommended } => {
            if packages.is_empty() {
                return Ok(Vec::new());
            }
            let conf = if *recommended {
                let written = sandbox
                    .run("mkdir -p target/warmd-full && cat > target/warmd-full/clippy.toml", Some(FULL_CLIPPY))
                    .await?;
                ensure!(written.ok(), "writing the full clippy.toml: {}", written.stderr.trim());
                "CLIPPY_CONF_DIR=\"$PWD/target/warmd-full\" "
            } else {
                ""
            };
            Ok(diag::parse(
                &sandbox
                    .run(&format!("{conf}{} {targets} {packages} -- {}", repo.cfg().clippy, args.join(" ")), None)
                    .await?
                    .stdout,
            ))
        }
        Step::Slapslop { args } => {
            Ok(sandbox.slapslop(&format!("cargo-slapslop run \"$PWD\" --diff HEAD~1 {}", args.join(" "))).await?.1)
        }
    }
}

fn changed_files(patch: &str) -> BTreeSet<String> {
    patch
        .lines()
        .filter_map(|line| line.strip_prefix("+++ b/").or_else(|| line.strip_prefix("--- a/")))
        .map(str::to_owned)
        .collect()
}
