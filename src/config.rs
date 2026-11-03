use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use serde::{Deserialize, de::DeserializeOwned};
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
    #[serde(default = "default_clippy")]
    pub(crate) clippy: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Job {
    pub(crate) name: String,
    pub(crate) cmd: String,
    pub(crate) fix: Option<String>,
    pub(crate) on_patch: Option<String>,
    #[serde(default)]
    pub(crate) args: String,
    #[serde(default)]
    pub(crate) slapslop: bool,
    #[serde(default)]
    pub(crate) warm_only: bool,
}

impl Job {
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

    pub(crate) fn answers_to(&self, key: &str) -> bool {
        self.name == key || self.name.strip_prefix(key).is_some_and(|rest| rest.starts_with('-'))
    }
}

#[derive(Debug, Default, Deserialize)]
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
    0
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

fn quote_all(args: &[String]) -> String {
    args.iter().map(|arg| format!("'{}'", arg.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ")
}
