use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use toml::from_str;

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
    pub(crate) repos: BTreeMap<String, RepoCfg>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoCfg {
    pub(crate) url: String,
    #[serde(default = "default_branch")]
    pub(crate) branch: String,
    pub(crate) jobs: Vec<Job>,
    pub(crate) slapslop: Option<String>,
    #[serde(default = "default_clippy")]
    pub(crate) clippy: String,
}

impl RepoCfg {
    pub(crate) fn all_jobs(&self) -> Vec<Job> {
        let slapslop = self.slapslop.iter().map(|cmd| Job {
            name: "slapslop".to_owned(),
            cmd: cmd.clone(),
            fix: None,
            slapslop: true,
        });
        self.jobs.iter().cloned().chain(slapslop).collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Job {
    pub(crate) name: String,
    pub(crate) cmd: String,
    pub(crate) fix: Option<String>,
    #[serde(skip)]
    pub(crate) slapslop: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub(crate) steps: Vec<Step>,
    pub(crate) repos: Option<Vec<String>>,
}

impl Profile {
    pub(crate) fn applies_to(&self, repo: &str) -> bool {
        self.repos.as_ref().is_none_or(|repos| repos.iter().any(|name| name == repo))
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Step {
    Clippy { targets: String, args: Vec<String>, conf_dir: Option<PathBuf> },
    Slapslop { args: Vec<String> },
}

pub(crate) fn load(dir: &Path) -> anyhow::Result<(Config, BTreeMap<String, Profile>)> {
    let config = read(&dir.join("repos.toml"))?;
    let profiles_path = dir.join("profiles.toml");
    let mut profiles: BTreeMap<String, Profile> =
        if profiles_path.exists() { read(&profiles_path)? } else { BTreeMap::new() };
    let root = fs::canonicalize(dir)?;
    for step in profiles.values_mut().flat_map(|profile| &mut profile.steps) {
        if let Step::Clippy { conf_dir: Some(conf), .. } = step {
            *conf = root.join(&*conf);
        }
    }
    Ok((config, profiles))
}

fn read<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

const fn default_poll_secs() -> u64 {
    60
}

const fn default_keep_hours() -> u64 {
    12
}

const fn default_jobs_per_repo() -> u16 {
    4
}

fn default_branch() -> String {
    "main".to_owned()
}

fn default_clippy() -> String {
    "cargo clippy --message-format=json".to_owned()
}
