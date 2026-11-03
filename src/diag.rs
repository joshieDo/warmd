use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::from_str;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Diag {
    pub(crate) level: String,
    pub(crate) code: Option<String>,
    pub(crate) file: String,
    pub(crate) line: u32,
    pub(crate) message: String,
    pub(crate) hazy: bool,
    pub(crate) rendered: String,
}

impl Diag {
    fn key(&self) -> Key {
        Key { code: self.code.clone(), file: self.file.clone(), message: self.message.clone() }
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    code: Option<String>,
    file: String,
    message: String,
}

#[derive(Deserialize)]
struct Line {
    reason: String,
    message: Option<Message>,
    fresh: Option<bool>,
}

#[derive(Deserialize)]
struct Message {
    level: String,
    message: String,
    code: Option<Code>,
    spans: Vec<Span>,
    rendered: Option<String>,
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

#[derive(Deserialize)]
struct Span {
    file_name: String,
    line_start: u32,
    is_primary: bool,
}

/// How many crates cargo really compiled (not fresh) in a JSON-message run; None when it printed no artifacts.
pub(crate) fn rebuilt(stdout: &str) -> Option<u32> {
    let artifacts = stdout
        .lines()
        .filter_map(|text| from_str::<Line>(text).ok())
        .filter(|line| line.reason == "compiler-artifact")
        .map(|line| line.fresh == Some(false))
        .collect::<Vec<_>>();
    (!artifacts.is_empty())
        .then(|| artifacts.iter().filter(|&&built| built).fold(0_u32, |count, _| count.saturating_add(1)))
}

pub(crate) fn parse(stdout: &str) -> Vec<Diag> {
    let mut seen = BTreeSet::new();
    for line in stdout.lines().filter_map(|text| from_str::<Line>(text).ok()) {
        let Some(message) = line.message.filter(|_| line.reason == "compiler-message") else { continue };
        let Some(span) = message.spans.iter().find(|span| span.is_primary) else { continue };
        let suffix = slapslop_suffix(&message.message);
        seen.insert(Diag {
            level: message.level.clone(),
            code: message.code.map(|code| code.code).or_else(|| suffix.map(|(lint, _)| lint.to_owned())),
            file: span.file_name.clone(),
            line: span.line_start,
            hazy: suffix.is_some_and(|(_, hazy)| hazy),
            message: message.message,
            rendered: message.rendered.unwrap_or_default(),
        });
    }
    seen.extend(rustfmt_diffs(stdout));
    seen.into_iter().collect()
}

pub(crate) fn new_only(found: Vec<Diag>, baseline: &[Diag]) -> Vec<Diag> {
    let mut known: BTreeMap<Key, usize> = BTreeMap::new();
    for diag in baseline {
        *known.entry(diag.key()).or_default() += 1;
    }
    found
        .into_iter()
        .filter(|diag| match known.get_mut(&diag.key()) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

pub(crate) fn added_lines(patch: &str) -> BTreeMap<String, BTreeSet<u32>> {
    let mut files: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut file = None;
    let mut next = 0;
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").map(str::to_owned);
        } else if let Some(hunk) = line.strip_prefix("@@ ") {
            next = hunk_start(hunk).unwrap_or(0);
        } else if line.starts_with('+') {
            if let Some(path) = &file {
                files.entry(path.clone()).or_default().insert(next);
            }
            next += 1;
        } else if line.starts_with(' ') {
            next += 1;
        }
    }
    files
}

fn rustfmt_diffs(stdout: &str) -> Vec<Diag> {
    let mut diags: Vec<Diag> = Vec::new();
    for line in stdout.lines() {
        if let Some(place) = line.strip_prefix("Diff in ").and_then(|rest| rest.strip_suffix(':')) {
            let Some((path, line_no)) =
                place.rsplit_once(':').and_then(|(path, number)| Some((path, number.parse().ok()?)))
            else {
                continue;
            };
            let file =
                path.strip_prefix("/tmp/warmd/").and_then(|rest| rest.split_once('/')).map_or(path, |(_, file)| file);
            diags.push(Diag {
                level: "error".to_owned(),
                code: Some("rustfmt".to_owned()),
                file: file.to_owned(),
                line: line_no,
                message: "not formatted".to_owned(),
                hazy: false,
                rendered: format!("Diff in {file}:{line_no}:\n"),
            });
        } else if let Some(last) = diags.last_mut() {
            last.rendered.push_str(line);
            last.rendered.push('\n');
        }
    }
    diags
}

fn slapslop_suffix(message: &str) -> Option<(&str, bool)> {
    let inner = message.strip_suffix(']')?.rsplit_once('[')?.1;
    let (lint, rest) = inner.split_once(';').unwrap_or((inner, ""));
    (!lint.is_empty() && lint.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'))
        .then(|| (lint, rest.trim_start().starts_with("hazy")))
}

fn hunk_start(hunk: &str) -> Option<u32> {
    (hunk.split(' ').find_map(|part| part.strip_prefix('+'))?).split(',').next()?.parse().ok()
}
