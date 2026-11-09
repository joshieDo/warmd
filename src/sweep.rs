use std::{
    cmp::Reverse,
    collections::HashMap,
    fs::{self, Metadata},
    io::ErrorKind,
    iter::once,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::Context as _;

use crate::read_dir_or_empty;

const INCREMENTAL_KEPT: usize = 2;

const DEPTH: usize = 5;

#[derive(Debug, Default)]
pub(crate) struct Swept {
    pub(crate) units: usize,
    pub(crate) incremental: usize,
}

impl Swept {
    pub(crate) fn sweep(target: &Path, cutoff: SystemTime) -> anyhow::Result<Self> {
        let mut swept = Self::default();
        walk(target, cutoff, DEPTH, &mut swept)?;
        Ok(swept)
    }
}

fn walk(dir: &Path, cutoff: SystemTime, depth: usize, swept: &mut Swept) -> anyhow::Result<()> {
    for entry in children(dir)? {
        if !entry.is_dir() || entry.is_symlink() {
            continue;
        }
        match entry.file_name().and_then(|name| name.to_str()) {
            Some(".fingerprint") => swept.units += sweep_flat(dir, cutoff)?,
            Some("build") if !dir.join(".fingerprint").is_dir() => swept.units += sweep_nested(&entry, cutoff)?,
            Some("incremental") => swept.incremental += sweep_incremental(&entry, cutoff)?,
            Some("build" | "deps" | "examples" | "out") => {}
            Some(_) | None if depth > 0 => walk(&entry, cutoff, depth - 1, swept)?,
            Some(_) | None => {}
        }
    }
    Ok(())
}

fn sweep_flat(profile: &Path, cutoff: SystemTime) -> anyhow::Result<usize> {
    let mut deps = HashMap::<String, Vec<_>>::new();
    for file in children(&profile.join("deps"))? {
        if let Some(hash) = file.file_name().and_then(|name| name.to_str()).and_then(unit_hash) {
            deps.entry(hash.to_owned()).or_default().push(file);
        }
    }
    let mut removed = 0;
    for unit in children(&profile.join(".fingerprint"))? {
        let Some(name) = unit.file_name().and_then(|name| name.to_str()).map(str::to_owned) else {
            continue;
        };
        let Some(hash) = unit_hash(&name) else {
            continue;
        };
        if used_since(&unit, cutoff)? {
            continue;
        }
        remove(&unit)?;
        remove(&profile.join("build").join(&name))?;
        for file in deps.get(hash).into_iter().flatten() {
            remove(file)?;
        }
        removed += 1;
    }
    Ok(removed)
}

fn sweep_nested(build: &Path, cutoff: SystemTime) -> anyhow::Result<usize> {
    let mut removed = 0;
    for package in children(build)? {
        for unit in children(&package)? {
            if !unit.file_name().and_then(|name| name.to_str()).is_some_and(is_hash) || !unit.is_dir() {
                continue;
            }
            let fingerprint = unit.join("fingerprint");
            if used_since(if fingerprint.is_dir() { &fingerprint } else { &unit }, cutoff)? {
                continue;
            }
            remove(&unit)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn sweep_incremental(dir: &Path, cutoff: SystemTime) -> anyhow::Result<usize> {
    let mut by_crate = HashMap::<String, Vec<_>>::new();
    for entry in children(dir)? {
        let Some((name, _)) = entry.file_name().and_then(|name| name.to_str()).and_then(|name| name.rsplit_once('-'))
        else {
            continue;
        };
        let modified = metadata(&entry)?.map(|meta| meta.modified()).transpose()?;
        by_crate.entry(name.to_owned()).or_default().push((modified, entry));
    }
    let mut removed = 0;
    for (_, mut dirs) in by_crate {
        dirs.sort_by_key(|(modified, _)| Reverse(*modified));
        for (modified, entry) in dirs.into_iter().skip(INCREMENTAL_KEPT) {
            if modified.is_none_or(|at| at < cutoff) {
                remove(&entry)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn used_since(dir: &Path, cutoff: SystemTime) -> anyhow::Result<bool> {
    for path in once(dir.to_owned()).chain(children(dir)?) {
        let Some(meta) = metadata(&path)? else { continue };
        if meta.accessed()?.max(meta.modified()?) >= cutoff {
            return Ok(true);
        }
    }
    Ok(false)
}

fn unit_hash(name: &str) -> Option<&str> {
    let (_, rest) = name.rsplit_once('-')?;
    let hash = rest.split('.').next()?;
    is_hash(hash).then_some(hash)
}

fn is_hash(text: &str) -> bool {
    text.len() == 16 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn children(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    read_dir_or_empty(dir)
}

fn metadata(path: &Path) -> anyhow::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn remove(path: &Path) -> anyhow::Result<()> {
    match if path.is_dir() && !path.is_symlink() { fs::remove_dir_all(path) } else { fs::remove_file(path) } {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}
