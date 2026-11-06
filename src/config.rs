use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    slice,
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use toml::from_str;

pub(crate) const FULL_CLIPPY: &str = include_str!("full-clippy.toml");

const FULL: &str = include_str!("full.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) listen: String,
    pub(crate) state_dir: PathBuf,
    #[serde(default = "default_poll_secs")]
    pub(crate) poll_secs: u64,
    #[serde(default = "default_keep_hours")]
    pub(crate) keep_hours: u64,
    #[serde(default = "default_jobs_per_repo")]
    pub(crate) jobs_per_repo: u16,
    #[serde(default = "default_lease_secs")]
    pub(crate) lease_secs: u64,
    pub(crate) repos: BTreeMap<String, RepoCfg>,
}

impl Config {
    pub(crate) fn load(dir: &Path) -> anyhow::Result<Self> {
        read(&dir.join("repos.toml"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoCfg {
    pub(crate) url: String,
    #[serde(default = "default_branch")]
    pub(crate) branch: String,
    pub(crate) jobs: Vec<Job>,
    #[serde(default = "default_clippy")]
    pub(crate) clippy: String,
    /// What `warmd build` runs, before its `-p`, `--bin` and `--release`: the toolchain and flags of the test job, so a
    /// debug build reuses main's warm test build.
    #[serde(default = "default_build")]
    pub(crate) build: String,
    pub(crate) ra: Option<RaCfg>,
}

impl RepoCfg {
    /// The slapslop jobs, each with what a main build runs.
    pub(crate) fn slapslop_main_cmds(&self) -> Vec<(&Job, String)> {
        self.jobs.iter().filter(|job| job.slapslop).map(|job| (job, job.main_cmd())).collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RaCfg {
    pub(crate) build: String,
    pub(crate) toolchain: Option<String>,
    #[serde(default)]
    pub(crate) all_features: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Job {
    pub(crate) name: String,
    pub(crate) cmd: String,
    pub(crate) fix: Option<String>,
    pub(crate) on_patch: Option<String>,
    /// What a main build runs instead of `cmd`; its hits are main's baseline (`warmd hits`).
    pub(crate) on_main: Option<String>,
    #[serde(default)]
    pub(crate) args: String,
    #[serde(default)]
    pub(crate) slapslop: bool,
    #[serde(default)]
    pub(crate) bins_only: bool,
    #[serde(default)]
    pub(crate) benches_only: bool,
    #[serde(default)]
    pub(crate) test: bool,
}

impl Job {
    /// What a main build runs: `on_main`, else `cmd`, rendered with the job's own args.
    pub(crate) fn main_cmd(&self) -> String {
        self.render(self.on_main.as_ref().unwrap_or(&self.cmd), None)
    }

    /// The lane the job runs in: slapslop, or its name up to the first `-` (`clippy-tests` runs with `clippy`).
    pub(crate) fn lane(&self) -> &str {
        if self.slapslop { "slapslop" } else { self.name.split_once('-').map_or(self.name.as_str(), |(head, _)| head) }
    }

    pub(crate) fn render(&self, template: &str, args: Option<&ArgsOverride>) -> String {
        let args = args.map_or_else(
            || self.args.clone(),
            |over| {
                let base = over.replace.as_ref().map_or_else(|| self.args.clone(), |replace| quote_all(replace));
                format!("{base} {}", quote_all(&over.add)).trim().to_owned()
            },
        );
        if template.contains("{args}") { template.replace("{args}", &args) } else { format!("{template} {args}") }
    }

    pub(crate) fn wanted(&self, tests: bool, only: &[String], skip: &[String], jobs: &[Self]) -> bool {
        if tests || self.test {
            return tests && self.test;
        }
        !skip.iter().any(|key| self.answers_to(key, jobs))
            && (only.is_empty() || only.iter().any(|key| self.answers_to(key, jobs)))
    }

    pub(crate) fn runs_named_tests(&self) -> bool {
        self.test && self.cmd.contains("{package}") && self.cmd.contains("{tests}")
    }

    pub(crate) fn answers_to(&self, key: &str, jobs: &[Self]) -> bool {
        self.name == key
            || (!jobs.iter().any(|job| job.name == key)
                && self.name.strip_prefix(key).is_some_and(|rest| rest.starts_with('-')))
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArgsOverride {
    #[serde(default)]
    add: Vec<String>,
    replace: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub(crate) steps: Vec<Step>,
}

impl Profile {
    pub(crate) fn full() -> anyhow::Result<Self> {
        from_str(FULL).context("parsing the built-in full profile")
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Step {
    Clippy {
        targets: String,
        args: Vec<String>,
        #[serde(default)]
        recommended: bool,
    },
    Slapslop {
        args: Vec<String>,
    },
}

impl Step {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Slapslop { .. } => "full:slapslop".to_owned(),
            Self::Clippy { targets, .. } if targets.contains("--lib --bins") => "full:clippy-non-test".to_owned(),
            Self::Clippy { .. } => "full:clippy".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tests {
    package: String,
    names: Vec<String>,
}

impl Tests {
    pub(crate) fn problem(&self) -> Option<String> {
        if let Some(problem) = name_problem("package", &self.package) {
            return Some(problem);
        }
        if self.names.is_empty() {
            return Some("name the tests to run: workspace or whole-package runs are refused".to_owned());
        }
        self.names
            .iter()
            .find(|name| name.trim().is_empty() || name.starts_with('-'))
            .map(|name| format!("test name {name:?} is empty or looks like a flag"))
    }

    pub(crate) fn fill(&self, cmd: &str) -> String {
        cmd.replace("{package}", &quote_all(slice::from_ref(&self.package))).replace("{tests}", &quote_all(&self.names))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binary {
    package: String,
    bin: Option<String>,
    #[serde(default)]
    release: bool,
    #[serde(default)]
    features: Vec<String>,
}

impl Binary {
    pub(crate) fn problem(&self) -> Option<String> {
        name_problem("package", &self.package)
            .or_else(|| self.bin.as_deref().and_then(|bin| name_problem("bin", bin)))
            .or_else(|| {
                self.features
                    .iter()
                    .find(|feature| feature.starts_with('-'))
                    .map(|feature| format!("feature {feature:?} looks like a flag"))
            })
    }

    pub(crate) const fn profile(&self) -> &'static str {
        if self.release { "release" } else { "debug" }
    }

    pub(crate) fn bin(&self) -> Option<&str> {
        self.bin.as_deref()
    }

    pub(crate) fn cmd(&self, build: &str) -> String {
        let mut cmd = format!(
            "{build} --message-format=json-render-diagnostics -p {}",
            quote_all(slice::from_ref(&self.package))
        );
        if let Some(bin) = &self.bin {
            cmd.push_str(&format!(" --bin {}", quote_all(slice::from_ref(bin))));
        }
        if self.release {
            cmd.push_str(" --release");
        }
        if !self.features.is_empty() {
            cmd.push_str(&format!(" --features {}", quote_all(slice::from_ref(&self.features.join(",")))));
        }
        cmd
    }
}

/// Why `name` is not a package or target name, if it is not.
fn name_problem(what: &str, name: &str) -> Option<String> {
    (name.is_empty()
        || name.starts_with('-')
        || !name.chars().all(|char| char.is_ascii_alphanumeric() || char == '-' || char == '_'))
    .then(|| format!("{what} {name:?} is not a {what} name"))
}

fn read<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    from_str(&(fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?))
        .with_context(|| format!("parsing {}", path.display()))
}

const fn default_poll_secs() -> u64 {
    60
}

const fn default_keep_hours() -> u64 {
    0
}

const fn default_lease_secs() -> u64 {
    60
}

const fn default_jobs_per_repo() -> u16 {
    4
}

fn default_branch() -> String {
    "main".to_owned()
}

fn default_build() -> String {
    "cargo build".to_owned()
}

fn default_clippy() -> String {
    "cargo clippy --message-format=json".to_owned()
}

fn quote_all(args: &[String]) -> String {
    args.iter().map(|arg| format!("'{}'", arg.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ")
}
