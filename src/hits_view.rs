//! The dashboard's view of main's hits: counts by job, lint and directory, then one file's hits, so the page never
//! loads a whole baseline (tempo's is tens of MB).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{diag::Diag, repo::Baseline};

/// Rows a list returns at most.
const LIMIT: usize = 60;

/// Hits a file returns at most.
const HITS_LIMIT: usize = 200;

#[derive(Debug, Serialize)]
pub(crate) struct HitsView<'a> {
    sha: &'a str,
    job: Option<&'a str>,
    jobs: Vec<Count>,
    shown: Count,
    lints: Vec<Count>,
    dirs: Vec<Count>,
    files: Vec<Count>,
    hits: Vec<Hit<'a>>,
    file_hits: Count,
}

/// What the page drilled into; every field narrows the hits the counts are over.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Filter {
    job: Option<String>,
    lint: Option<String>,
    dir: Option<String>,
    file: Option<String>,
}

#[derive(Debug, Serialize)]
struct Count {
    name: String,
    total: u64,
    hazy: u64,
}

impl Count {
    fn of<'a>(name: &str, hits: impl IntoIterator<Item = &'a Diag>) -> Self {
        let mut count = Self { name: name.to_owned(), total: 0, hazy: 0 };
        for hit in hits {
            count.add(hit);
        }
        count
    }

    fn add(&mut self, hit: &Diag) {
        self.total += 1;
        self.hazy += u64::from(hit.hazy);
    }
}

#[derive(Debug, Serialize)]
struct Hit<'a> {
    line: u32,
    hazy: bool,
    lint: Option<&'a str>,
    rendered: &'a str,
}

pub(crate) fn view<'a>(sha: &'a str, baseline: &'a Baseline, filter: &Filter) -> HitsView<'a> {
    let job = filter
        .job
        .as_deref()
        .and_then(|name| baseline.get_key_value(name))
        .or_else(|| baseline.iter().max_by_key(|(_, hits)| hits.len()))
        .map(|(name, _)| name.as_str());
    let all = job.and_then(|name| baseline.get(name)).map_or(&[][..], Vec::as_slice);
    let by_lint = |hit: &&Diag| filter.lint.as_deref().is_none_or(|lint| hit.lint() == lint);
    let by_dir = |hit: &&Diag| filter.dir.as_deref().is_none_or(|dir| dir_of(&hit.file) == dir);
    let in_lint = all.iter().filter(by_lint).collect::<Vec<_>>();
    let both = in_lint.iter().copied().filter(by_dir).collect::<Vec<_>>();
    let mut hits = filter
        .file
        .as_deref()
        .map_or_else(Vec::new, |file| both.iter().copied().filter(|hit| hit.file == file).collect::<Vec<_>>());
    hits.sort_by_key(|hit| hit.line);
    HitsView {
        sha,
        job,
        jobs: baseline.iter().map(|(name, hits)| Count::of(name, hits)).collect(),
        shown: Count::of("shown", both.iter().copied()),
        lints: counts(&all.iter().filter(by_dir).collect::<Vec<_>>(), Diag::lint),
        dirs: counts(&in_lint, |hit| dir_of(&hit.file)),
        files: counts(&both, |hit| hit.file.as_str()),
        file_hits: Count::of("file", hits.iter().copied()),
        hits: hits
            .into_iter()
            .take(HITS_LIMIT)
            .map(|hit| Hit { line: hit.line, hazy: hit.hazy, lint: hit.code.as_deref(), rendered: &hit.rendered })
            .collect(),
    }
}

/// The directory a file counts under: its crate (`crates/<name>`, `bin/<name>`, ..) or its top directory.
fn dir_of(file: &str) -> &str {
    let depth = if matches!(file.split('/').next(), Some("crates" | "bin" | "lib" | "packages")) { 2 } else { 1 };
    file.match_indices('/').nth(depth - 1).map_or(".", |(at, _)| file.get(..at).unwrap_or(file))
}

/// Counts of `hits` by `key`, largest first, at most [`LIMIT`].
fn counts<'a>(hits: &[&'a Diag], key: impl Fn(&'a Diag) -> &'a str) -> Vec<Count> {
    let mut by: BTreeMap<&str, Count> = BTreeMap::new();
    for hit in hits {
        by.entry(key(hit)).or_insert_with_key(|name| Count::of(name, [])).add(hit);
    }
    let mut list = by.into_values().collect::<Vec<_>>();
    list.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.name.cmp(&b.name)));
    list.truncate(LIMIT);
    list
}
