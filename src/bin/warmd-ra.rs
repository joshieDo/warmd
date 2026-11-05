use std::{
    collections::{HashMap, HashSet},
    env,
    fmt::Debug,
    fs::File,
    io::{self, BufRead as _, BufReader, Read as _, Write as _},
    mem::ManuallyDrop,
    os::fd::{AsRawFd as _, FromRawFd as _},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Instant,
};

use anyhow::{Context as _, Error, anyhow, bail, ensure};
use libc::{
    _exit, O_CLOEXEC, POLLIN, SIGKILL, WNOHANG, alarm, dup2, fork, kill, nfds_t, pid_t, pipe2, poll, pollfd, waitpid,
};
use ra_ap_hir::{ChangeWithProcMacros, Semantics, attach_db};
use ra_ap_ide::{AnalysisHost, Diagnostic, DiagnosticsConfig, FileId, RootDatabase, Severity};
use ra_ap_ide_db::{assists::AssistResolveStrategy, defs::Definition};
use ra_ap_load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_at};
use ra_ap_paths::AbsPathBuf;
use ra_ap_proc_macro_api::ProcMacroClient;
use ra_ap_project_model::{CargoConfig, CargoFeatures, RustLibSource};
use ra_ap_syntax::{AstNode as _, Edition, NodeOrToken, SourceFile, SyntaxKind, SyntaxNode, TextRange, ast};
use ra_ap_vfs::{Vfs, VfsPath};
use serde_json::{Value, from_slice, from_str, json};

const POOL: usize = 2;

const CHILD_THREADS: usize = 8;

const CHILD_TIMEOUT_SECS: u32 = 300;

const HANDSHAKE: [&str; 2] = [r#"{"ApiVersionCheck":{}}"#, r#"{"SetConfig":{"span_mode":"RustAnalyzer"}}"#];

struct PmPipe {
    exe: String,
    to_server: i32,
    from_server: i32,
}

struct Idle {
    pid: pid_t,
    request: File,
    reply: File,
}

struct Busy {
    pid: pid_t,
    id: Value,
    reply: File,
    got: Vec<u8>,
}

struct Hunk {
    old_start: usize,
    lines: Vec<String>,
}

struct Warm {
    host: AnalysisHost,
    vfs: Vfs,
    root: String,
    cfg: DiagnosticsConfig,
    baseline: HashMap<FileId, Vec<String>>,
    local: HashSet<FileId>,
    pm: Vec<PmPipe>,
    threads: usize,
    pm_client: Option<ProcMacroClient>,
}

impl Warm {
    fn load() -> anyhow::Result<Self> {
        let build = env::var("WARMD_RA_BUILD").context("WARMD_RA_BUILD is unset")?;
        let root = env::current_dir()?;
        let threads = thread::available_parallelism().map_or(CHILD_THREADS, usize::from);
        let mut cargo = CargoConfig {
            all_targets: true,
            sysroot: Some(RustLibSource::Discover),
            set_test: true,
            run_build_script_command: Some(vec!["sh".to_owned(), "-c".to_owned(), build]),
            ..CargoConfig::default()
        };
        if env::var_os("WARMD_RA_ALL_FEATURES").is_some() {
            cargo.features = CargoFeatures::All;
        }
        let load = LoadCargoConfig {
            load_out_dirs_from_check: true,
            with_proc_macro_server: ProcMacroServerChoice::Sysroot,
            prefill_caches: true,
            num_worker_threads: threads,
            proc_macro_processes: 1,
        };
        let (db, vfs, client) = load_workspace_at(&root, &cargo, &load, &|_| {})?;
        let root = root.to_string_lossy().into_owned();
        let mut cfg = DiagnosticsConfig::test_sample();
        cfg.proc_macros_enabled = true;
        cfg.proc_attr_macros_enabled = true;
        cfg.disable_experimental = false;
        let mut warm = Self {
            host: AnalysisHost::with_database(db),
            vfs,
            root,
            cfg,
            baseline: HashMap::new(),
            local: HashSet::new(),
            pm: Vec::new(),
            threads,
            pm_client: client,
        };
        let analysis = warm.host.analysis();
        let prefix = format!("{}/", warm.root);
        warm.local = warm
            .vfs
            .iter()
            .filter(|(_, path)| {
                let path = path.to_string();
                path.starts_with(&prefix)
                    && !path.contains("/target/")
                    && Path::new(&path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
            })
            .map(|(file, _)| file)
            .filter(|file| analysis.crates_for(*file).is_ok_and(|crates| !crates.is_empty()))
            .collect();
        drop(analysis);
        let all: Vec<FileId> = warm.local.iter().copied().collect();
        let baseline = warm.diagnose(&all, threads);
        warm.baseline = baseline.into_iter().map(|(file, diags)| (file, diags.iter().map(key).collect())).collect();
        // The first revision after warming evicts every memo past its LRU cap: pay that once here, not in each fork.
        warm.host.trigger_cancellation();
        warm.pm = pm_pipes();
        Ok(warm)
    }

    fn ready_stats(&self, start: Instant) -> Value {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        json!({
            "secs": start.elapsed().as_secs(),
            "files": self.local.len(),
            "rss_mb": kb(&status, "VmRSS:") / 1024,
            "main_errors": self.baseline.values().flatten().filter(|diag| diag.starts_with("error|")).count(),
            "proc_macro_servers": self.pm.len(),
            "proc_macros": self.pm_client.is_some(),
        })
    }

    fn diagnose(&self, files: &[FileId], threads: usize) -> Vec<(FileId, Vec<Diagnostic>)> {
        let chunk = files.len().div_ceil(threads.max(1)).max(1);
        thread::scope(|scope| {
            let workers: Vec<_> = files
                .chunks(chunk)
                .map(|part| {
                    let analysis = self.host.analysis();
                    let cfg = &self.cfg;
                    scope.spawn(move || {
                        part.iter()
                            .map(|file| {
                                let diags = analysis.full_diagnostics(cfg, AssistResolveStrategy::None, *file);
                                let own =
                                    diags.unwrap_or_default().into_iter().filter(|diag| diag.range.file_id == *file);
                                (*file, own.collect())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().flat_map(|worker| worker.join().unwrap_or_default()).collect()
        })
    }

    fn patched(&self, patch: &str) -> anyhow::Result<Vec<(FileId, String)>> {
        let analysis = self.host.analysis();
        hunks(patch)?
            .into_iter()
            .map(|(path, file_hunks)| {
                let abs = AbsPathBuf::assert_utf8(format!("{}/{path}", self.root).into());
                let (file, _) =
                    self.vfs.file_id(&VfsPath::from(abs)).with_context(|| format!("{path} is not loaded"))?;
                let old = analysis.file_text(file).map_err(cancelled)?;
                Ok((file, apply(&old, &file_hunks).with_context(|| path.clone())?))
            })
            .collect()
    }

    fn affected(&self, files: &[(FileId, String)]) -> anyhow::Result<Vec<FileId>> {
        let mut affected: Vec<FileId> = files.iter().map(|(file, _)| *file).collect();
        let db = self.host.raw_database();
        let analysis = self.host.analysis();
        attach_db(db, || {
            let sema = Semantics::new(db);
            for (file, new_text) in files {
                let old_text = analysis.file_text(*file).map_err(cancelled)?;
                let old = signatures(&SourceFile::parse(&old_text, Edition::CURRENT).tree());
                let new = signatures(&SourceFile::parse(new_text, Edition::CURRENT).tree());
                let changed: Vec<&String> = old
                    .iter()
                    .filter(|(name, (sig, _))| new.get(*name).is_none_or(|(now, _)| now != sig))
                    .map(|(name, _)| name)
                    .collect();
                if changed.is_empty() {
                    continue;
                }
                let live = signatures(&sema.parse_guess_edition(*file));
                for name in changed {
                    let Some(def) = live.get(name).and_then(|(_, node)| definition(&sema, node)) else { continue };
                    for user in def.usages(&sema).all().references.into_keys() {
                        let user = user.file_id(db);
                        if self.local.contains(&user) && !affected.contains(&user) {
                            affected.push(user);
                        }
                    }
                }
            }
            Ok(affected)
        })
    }

    fn change(&mut self, patch: &str) -> anyhow::Result<Vec<FileId>> {
        let files = self.patched(patch)?;
        let affected = self.affected(&files)?;
        let mut change = ChangeWithProcMacros::default();
        for (file, text) in files {
            change.change_file(file, Some(text));
        }
        self.host.apply_change(change);
        Ok(affected)
    }

    fn check(&mut self, patch: &str) -> anyhow::Result<Value> {
        let affected = self.change(patch)?;
        let analysis = self.host.analysis();
        let mut found = Vec::new();
        for (file, diags) in self.diagnose(&affected, CHILD_THREADS) {
            let lines = analysis.file_line_index(file).map_err(cancelled)?;
            let path = self.vfs.file_path(file).to_string();
            let path = path.strip_prefix(&format!("{}/", self.root)).unwrap_or(&path).to_owned();
            let mut on_main = self.baseline.get(&file).cloned().unwrap_or_default();
            for diag in diags.iter().filter(|diag| matches!(diag.severity, Severity::Error | Severity::Warning)) {
                if let Some(seen) = on_main.iter().position(|old| *old == key(diag)) {
                    on_main.swap_remove(seen);
                    continue;
                }
                let at = lines.line_col(diag.range.range.start());
                found.push(json!({
                    "file": path, "line": at.line + 1, "col": at.col + 1, "severity": severity(diag.severity),
                    "code": diag.code.as_str(), "message": diag.message,
                }));
            }
        }
        Ok(json!({ "files": affected.len(), "diagnostics": found }))
    }

    fn advance(&mut self, patch: &str) -> anyhow::Result<Value> {
        let affected = self.change(patch)?;
        for (file, diags) in self.diagnose(&affected, self.threads) {
            self.baseline.insert(file, diags.iter().map(key).collect());
        }
        self.host.trigger_cancellation();
        Ok(json!({ "files": affected.len() }))
    }

    fn fork(&mut self) -> anyhow::Result<Idle> {
        let (request_out, request_in) = pipe()?;
        let (reply_out, reply_in) = pipe()?;
        // SAFETY: no query runs and no lock is held: the parent only forks between requests, from this thread.
        let pid = unsafe { fork() };
        ensure!(pid >= 0, "fork: {}", io::Error::last_os_error());
        if pid == 0 {
            drop((request_in, reply_out));
            self.child(request_out, reply_in);
        }
        Ok(Idle { pid, request: request_in, reply: reply_out })
    }

    fn child(&mut self, request: File, mut reply: File) -> ! {
        // SAFETY: alarm only schedules SIGALRM for this process, which ends a child that hangs.
        unsafe { alarm(CHILD_TIMEOUT_SECS) };
        let ready = self.pm.iter().try_for_each(own_pm_server);
        let mut line = String::new();
        let read = BufReader::new(request).read_line(&mut line);
        let msg: Value = from_str(&line).unwrap_or_default();
        let start = Instant::now();
        let result = match (ready, read) {
            (Err(err), _) => Err(err),
            (_, Err(err)) => Err(err.into()),
            (Ok(()), Ok(_)) => catch_unwind(AssertUnwindSafe(|| self.check(msg["patch"].as_str().unwrap_or_default())))
                .unwrap_or_else(|_| Err(anyhow!("rust-analyzer panicked"))),
        };
        let mut out = result.unwrap_or_else(|err| json!({ "error": format!("{err:#}") }));
        out["id"] = msg["id"].clone();
        out["ms"] = json!(ms(start));
        if let Err(err) = writeln!(reply, "{out}") {
            eprintln!("warmd-ra: replying: {err}");
        }
        // SAFETY: _exit skips the destructors and atexit handlers of state the parent still owns.
        unsafe { _exit(0) }
    }

    fn serve(&mut self) -> anyhow::Result<()> {
        // SAFETY: fd 0 is this process's stdin; ManuallyDrop never closes it.
        let mut input = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
        let mut pending = Vec::new();
        let mut idle: Vec<Idle> = Vec::new();
        let mut busy: Vec<Busy> = Vec::new();
        let mut chunk = vec![0; 1 << 16];
        loop {
            while idle.len() < POOL {
                idle.push(self.fork()?);
            }
            // SAFETY: waitpid with WNOHANG only reaps children that already exited.
            while unsafe { waitpid(-1, std::ptr::null_mut(), WNOHANG) } > 0 {}
            let mut polled: Vec<pollfd> = std::iter::once(0)
                .chain(busy.iter().map(|job| job.reply.as_raw_fd()))
                .map(|fd| pollfd { fd, events: POLLIN, revents: 0 })
                .collect();
            let count = nfds_t::try_from(polled.len())?;
            // SAFETY: polled holds count initialized pollfd entries.
            if unsafe { poll(polled.as_mut_ptr(), count, -1) } < 0 {
                continue;
            }
            for (job, poll) in busy.iter_mut().zip(&polled[1..]) {
                if poll.revents != 0 {
                    let read = job.reply.read(&mut chunk).unwrap_or_default();
                    job.got.extend_from_slice(chunk.get(..read).unwrap_or_default());
                    if read == 0 {
                        job.pid = 0;
                    }
                }
            }
            for mut job in busy.extract_if(.., |job| job.pid == 0) {
                let got = std::mem::take(&mut job.got);
                let text = String::from_utf8_lossy(&got);
                match from_str::<Value>(text.trim()) {
                    Ok(reply) => say(&reply)?,
                    Err(_) => say(&json!({ "id": job.id, "error": "the check's process died" }))?,
                }
            }
            if polled[0].revents == 0 {
                continue;
            }
            let read = input.read(&mut chunk)?;
            if read == 0 {
                for child in idle.iter().map(|child| child.pid).chain(busy.iter().map(|job| job.pid)) {
                    // SAFETY: kill only signals our own children.
                    unsafe { kill(child, SIGKILL) };
                }
                return Ok(());
            }
            pending.extend_from_slice(chunk.get(..read).unwrap_or_default());
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let msg: Value = from_slice(&line).unwrap_or_default();
                if let Some(check) = msg.get("check") {
                    let mut child = match idle.pop() {
                        Some(child) => child,
                        None => self.fork()?,
                    };
                    writeln!(child.request, "{check}")?;
                    busy.push(Busy { pid: child.pid, id: check["id"].clone(), reply: child.reply, got: Vec::new() });
                } else if let Some(advance) = msg.get("advance") {
                    for child in std::mem::take(&mut idle) {
                        // SAFETY: kill only signals our own idle child, forked before main moved.
                        unsafe { kill(child.pid, SIGKILL) };
                    }
                    let start = Instant::now();
                    let mut out = self
                        .advance(advance["patch"].as_str().unwrap_or_default())
                        .unwrap_or_else(|err| json!({ "error": format!("{err:#}") }));
                    out["id"] = advance["id"].clone();
                    out["ms"] = json!(ms(start));
                    say(&out)?;
                }
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    let start = Instant::now();
    let mut warm = Warm::load()?;
    let ready = warm.ready_stats(start);
    say(&json!({ "ready": ready }))?;
    warm.serve()
}

fn say(value: &Value) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{value}")?;
    out.flush()?;
    Ok(())
}

fn cancelled(why: impl Debug) -> Error {
    anyhow!("rust-analyzer cancelled the query: {why:?}")
}

fn ms(since: Instant) -> u128 {
    since.elapsed().as_millis()
}

const fn severity(level: Severity) -> &'static str {
    match level {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::WeakWarning => "weak",
        Severity::Allow => "allow",
    }
}

fn key(diag: &Diagnostic) -> String {
    format!("{}|{}|{}", severity(diag.severity), diag.code.as_str(), diag.message)
}

fn kb(text: &str, field: &str) -> u64 {
    text.lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    // SAFETY: fds has room for the two descriptors pipe2 writes.
    if unsafe { pipe2(fds.as_mut_ptr(), O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 just created both descriptors and nothing else owns them.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

fn link(path: &str) -> String {
    std::fs::read_link(path).map_or_default(|target| target.to_string_lossy().into_owned())
}

fn pm_pipes() -> Vec<PmPipe> {
    let me = std::process::id();
    let ours: Vec<(i32, String)> = std::fs::read_dir("/proc/self/fd")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| Some((entry.file_name().to_str()?.parse().ok()?, link(&entry.path().to_string_lossy()))))
        .collect();
    let fd_for = |target: &str| ours.iter().find(|(_, to)| to == target).map(|(fd, _)| *fd);
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let parent = stat.rsplit(')').next().and_then(|rest| rest.split_whitespace().nth(1)?.parse::<u32>().ok());
        let exe = link(&format!("/proc/{pid}/exe"));
        if parent != Some(me) || !exe.contains("proc-macro-srv") {
            continue;
        }
        if let (Some(to_server), Some(from_server)) =
            (fd_for(&link(&format!("/proc/{pid}/fd/0"))), fd_for(&link(&format!("/proc/{pid}/fd/1"))))
        {
            found.push(PmPipe { exe, to_server, from_server });
        }
    }
    found
}

fn own_pm_server(pm: &PmPipe) -> anyhow::Result<()> {
    let mut server = Command::new(&pm.exe)
        .env("RUST_ANALYZER_INTERNALS_DO_NOT_USE", "this is unstable")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut to_server = server.stdin.take().context("proc-macro server stdin")?;
    let mut from_server = BufReader::new(server.stdout.take().context("proc-macro server stdout")?);
    let mut answer = String::new();
    for request in HANDSHAKE {
        writeln!(to_server, "{request}")?;
        to_server.flush()?;
        answer.clear();
        from_server.read_line(&mut answer)?;
    }
    ensure!(answer.contains("RustAnalyzer"), "proc-macro server refused span mode: {answer}");
    ensure!(from_server.buffer().is_empty(), "proc-macro server sent more than the handshake");
    // SAFETY: dup2 onto descriptors this process owns; rust-analyzer's client keeps using the same numbers.
    let spliced = unsafe {
        dup2(to_server.as_raw_fd(), pm.to_server) >= 0 && dup2(from_server.get_ref().as_raw_fd(), pm.from_server) >= 0
    };
    ensure!(spliced, "dup2: {}", io::Error::last_os_error());
    Ok(())
}

fn hunks(patch: &str) -> anyhow::Result<Vec<(String, Vec<Hunk>)>> {
    let mut files: Vec<(String, Vec<Hunk>)> = Vec::new();
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("diff --git ") {
            continue;
        }
        if let Some(old) = line.strip_prefix("--- ") {
            let new = lines.next().and_then(|next| next.strip_prefix("+++ ")).context("--- without +++")?;
            ensure!(old != "/dev/null" && new != "/dev/null", "adds or deletes a file");
            ensure!(old.strip_prefix("a/") == new.strip_prefix("b/"), "renames {old} to {new}");
            files.push((new.strip_prefix("b/").unwrap_or(new).to_owned(), Vec::new()));
            continue;
        }
        if let Some(range) = line.strip_prefix("@@ -") {
            let old_start = range.split([',', ' ']).next().and_then(|start| start.parse().ok()).context("bad hunk")?;
            let mut hunk = Hunk { old_start, lines: Vec::new() };
            while let Some(body) =
                lines.next_if(|next| next.starts_with([' ', '+', '-', '\\']) && !next.starts_with("--- "))
            {
                hunk.lines.push(body.to_owned());
            }
            files.last_mut().context("hunk before a file header")?.1.push(hunk);
            continue;
        }
        if line.starts_with("Binary files") || line.starts_with("rename ") || line.starts_with("new file") {
            bail!("unsupported patch line: {line}");
        }
    }
    Ok(files)
}

fn apply(old: &str, file_hunks: &[Hunk]) -> anyhow::Result<String> {
    let base: Vec<&str> = old.split_inclusive('\n').collect();
    let mut out = String::with_capacity(old.len());
    let mut next = 0;
    for hunk in file_hunks {
        let at = hunk.old_start.saturating_sub(1).max(next);
        out.extend(base.get(next..at).context("hunk past the end")?.iter().copied());
        next = at;
        for line in &hunk.lines {
            let (mark, text) = line.split_at(1);
            match mark {
                " " | "-" => {
                    let have = base.get(next).context("hunk past the end")?;
                    ensure!(have.trim_end_matches('\n') == text, "context mismatch at line {}", next + 1);
                    if mark == " " {
                        out.push_str(have);
                    }
                    next += 1;
                }
                "+" => {
                    out.push_str(text);
                    out.push('\n');
                }
                _ => {
                    if out.ends_with('\n') && line.starts_with("\\ No newline") {
                        out.pop();
                    }
                }
            }
        }
    }
    out.extend(base.get(next..).unwrap_or_default().iter().copied());
    Ok(out)
}

fn item_name(node: &SyntaxNode, item: &ast::Item) -> String {
    if let ast::Item::Impl(imp) = item {
        let text = |node: Option<&SyntaxNode>| node.map_or_default(|node| node.text().to_string());
        return format!(
            "impl {} for {}",
            text(imp.trait_().as_ref().map(ast::Type::syntax)),
            text(imp.self_ty().as_ref().map(ast::Type::syntax))
        );
    }
    ast::AnyHasName::cast(node.clone())
        .and_then(|named| ast::HasName::name(&named))
        .map_or_else(|| tokens(node, None), |name| name.text().to_string())
}

fn tokens(node: &SyntaxNode, skip: Option<TextRange>) -> String {
    node.descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| !token.kind().is_trivia() && skip.is_none_or(|range| !range.contains_range(token.text_range())))
        .map(|token| token.text().to_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn signatures(file: &SourceFile) -> HashMap<String, (String, SyntaxNode)> {
    let mut found = HashMap::new();
    for node in file.syntax().descendants() {
        let Some(item) = ast::Item::cast(node.clone()) else { continue };
        if node.ancestors().skip(1).any(|outer| outer.kind() == SyntaxKind::BLOCK_EXPR) {
            continue;
        }
        let members = match &item {
            ast::Item::Fn(func) => func.body().map(|body| body.syntax().text_range()),
            ast::Item::Impl(imp) => imp.assoc_item_list().map(|list| list.syntax().text_range()),
            ast::Item::Trait(tr) => tr.assoc_item_list().map(|list| list.syntax().text_range()),
            ast::Item::Module(module) => module.item_list().map(|list| list.syntax().text_range()),
            _ => None,
        };
        let path: Vec<String> = node
            .ancestors()
            .skip(1)
            .filter_map(|outer| {
                let outer_item = ast::Item::cast(outer.clone())?;
                matches!(outer_item, ast::Item::Module(_) | ast::Item::Impl(_) | ast::Item::Trait(_))
                    .then(|| item_name(&outer, &outer_item))
            })
            .collect();
        let base = format!("{}::{:?}::{}", path.join("::"), node.kind(), item_name(&node, &item));
        let mut name = base.clone();
        let mut twin = 1;
        while found.contains_key(&name) {
            twin += 1;
            name = format!("{base}#{twin}");
        }
        found.insert(name, (tokens(&node, members), node));
    }
    found
}

fn definition<'db>(sema: &Semantics<'db, RootDatabase>, node: &SyntaxNode) -> Option<Definition<'db>> {
    Some(match ast::Item::cast(node.clone())? {
        ast::Item::Fn(item) => Definition::Function(sema.to_def(&item)?),
        ast::Item::Struct(item) => Definition::Adt(sema.to_def(&item)?.into()),
        ast::Item::Enum(item) => Definition::Adt(sema.to_def(&item)?.into()),
        ast::Item::Union(item) => Definition::Adt(sema.to_def(&item)?.into()),
        ast::Item::Trait(item) => Definition::Trait(sema.to_def(&item)?),
        ast::Item::TypeAlias(item) => Definition::TypeAlias(sema.to_def(&item)?),
        ast::Item::Const(item) => Definition::Const(sema.to_def(&item)?),
        ast::Item::Static(item) => Definition::Static(sema.to_def(&item)?),
        ast::Item::Module(item) => Definition::Module(sema.to_def(&item)?),
        ast::Item::MacroRules(item) => Definition::Macro(sema.to_def(&ast::Macro::from(item))?),
        _ => return None,
    })
}
