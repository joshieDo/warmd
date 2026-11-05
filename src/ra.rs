use std::{
    collections::HashMap,
    env,
    path::Path,
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, bail, ensure};
use serde_json::{Value, from_str, json};
use tokio::{
    fs::create_dir_all,
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout},
    spawn,
    sync::{Mutex as AsyncMutex, oneshot},
    time::timeout,
};

use crate::{
    config::RaCfg,
    repo::{self, Gen, Repo},
    sandbox::{Sandbox, Tree},
};

const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

const ADVANCE_FILES: usize = 200;

const RELOAD_ON: [&str; 4] = ["Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain.toml"];

#[derive(Debug, Default)]
pub(crate) struct Tier1 {
    server: Mutex<Option<Arc<Server>>>,
}

impl Tier1 {
    pub(crate) fn view(&self) -> Value {
        self.current().map_or_else(
            || json!({ "state": "off" }),
            |server| {
                let mut view = server.state();
                view["sha"] = json!(server.sha());
                view
            },
        )
    }

    pub(crate) async fn check(&self, base: &str, patch: &str) -> Option<Value> {
        let server = self.current()?;
        if server.sha() != base {
            return Some(json!({ "skipped": format!("tier 1 is at main {}, not at the patch's base", server.sha()) }));
        }
        let state = server.state()["state"].clone();
        if state != "ready" {
            return Some(json!({ "skipped": format!("tier 1 is {state}") }));
        }
        Some(server.ask("check", patch).await.unwrap_or_else(|err| json!({ "error": format!("{err:#}") })))
    }

    pub(crate) async fn follow(&self, repo: &Repo, generation: Arc<Gen>, keep: Duration) {
        let Some(cfg) = repo.cfg().ra.clone() else {
            self.stop().await;
            return;
        };
        if let Some(server) = self.current() {
            let state = server.state()["state"].clone();
            if server.sha() == generation.sha() || state == "loading" {
                return;
            }
            if state == "ready" && server.generation.born().elapsed().is_ok_and(|age| age < keep) {
                match server.advance(&generation).await {
                    Ok(()) => return,
                    Err(err) => {
                        eprintln!("warmd: {} tier 1: loading {} afresh: {err:#}", repo.name(), generation.sha());
                    }
                }
            }
        }
        if let Err(err) = self.start(repo, &cfg, generation).await {
            eprintln!("warmd: {} tier 1: {err:#}", repo.name());
        }
    }

    async fn start(&self, repo: &Repo, cfg: &RaCfg, generation: Arc<Gen>) -> anyhow::Result<()> {
        self.stop().await;
        let layer = repo.jobs_dir().join("tier1");
        repo::remove_tree(&layer).await?;
        for dir in ["upper", "work"] {
            create_dir_all(layer.join(dir)).await?;
        }
        let tool = env::current_exe()?.with_file_name("warmd-ra");
        let mut vars = vec![("WARMD_RA_BUILD", cfg.build.as_str()), ("CARGO_NET_OFFLINE", "true")];
        if let Some(toolchain) = &cfg.toolchain {
            vars.push(("RUSTUP_TOOLCHAIN", toolchain));
        }
        if cfg.all_features {
            vars.push(("WARMD_RA_ALL_FEATURES", "1"));
        }
        let cache = repo.cache_dir();
        let tree = Tree::Overlay { lower: generation.dir(), layer: &layer, cache: &cache };
        let mut child = Sandbox { repo: repo.name(), tree }.serve(&tool, &vars).await?;
        let stdin = child.stdin.take().context("tier 1 stdin")?;
        let stdout = child.stdout.take().context("tier 1 stdout")?;
        let stderr = child.stderr.take().context("tier 1 stderr")?;
        let server = Arc::new(Server {
            sha: Mutex::new(generation.sha().to_owned()),
            generation,
            state: Mutex::new(json!({ "state": "loading" })),
            stdin: AsyncMutex::new(stdin),
            waiting: Mutex::default(),
            next: AtomicU64::new(0),
            child: AsyncMutex::new(child),
        });
        spawn(log(repo.name().to_owned(), stderr));
        spawn(listen(Arc::downgrade(&server), stdout));
        *self.server.lock().unwrap_or_else(PoisonError::into_inner) = Some(server);
        Ok(())
    }

    async fn stop(&self) {
        let old = self.server.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(server) = old
            && let Err(err) = server.child.lock().await.kill().await
        {
            eprintln!("warmd: stopping tier 1: {err}");
        }
    }

    fn current(&self) -> Option<Arc<Server>> {
        self.server.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

#[derive(Debug)]
struct Server {
    generation: Arc<Gen>,
    sha: Mutex<String>,
    state: Mutex<Value>,
    stdin: AsyncMutex<ChildStdin>,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next: AtomicU64,
    child: AsyncMutex<Child>,
}

impl Server {
    fn sha(&self) -> String {
        self.sha.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn state(&self) -> Value {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    async fn ask(&self, kind: &str, patch: &str) -> anyhow::Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner).insert(id, reply);
        let line = format!("{}\n", json!({ kind: { "id": id, "patch": patch } }));
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }
        let answer = timeout(REPLY_TIMEOUT, answer).await.context("tier 1 timed out")?.context("tier 1 exited")?;
        Ok(answer)
    }

    async fn advance(&self, generation: &Gen) -> anyhow::Result<()> {
        let from = self.sha();
        let dir = generation.dir();
        let names = repo::git(dir, &["diff", "--name-only", "--no-renames", &from, generation.sha()]).await?;
        let names: Vec<&str> = names.lines().collect();
        ensure!(names.len() <= ADVANCE_FILES, "{} files changed", names.len());
        if let Some(name) = names.iter().find(|name| RELOAD_ON.iter().any(|file| name.ends_with(file))) {
            bail!("{name} changed");
        }
        let rust: Vec<&str> = names
            .into_iter()
            .filter(|name| Path::new(name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("rs")))
            .collect();
        if !rust.is_empty() {
            let mut args =
                vec!["diff", "--no-renames", "--no-ext-diff", "--no-color", "--src-prefix=a/", "--dst-prefix=b/"];
            args.extend([from.as_str(), generation.sha(), "--"]);
            args.extend(rust);
            let answer = self.ask("advance", &repo::git(dir, &args).await?).await?;
            if let Some(err) = answer.get("error") {
                bail!("{err}");
            }
        }
        generation.sha().clone_into(&mut self.sha.lock().unwrap_or_else(PoisonError::into_inner));
        Ok(())
    }
}

pub(crate) fn tool_dir(tool: &Path) -> anyhow::Result<&Path> {
    tool.parent().context("warmd-ra has no directory")
}

async fn listen(server: Weak<Server>, stdout: ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(server) = server.upgrade() else { return };
        let Ok(mut msg) = from_str::<Value>(&line) else { continue };
        if let Some(ready) = msg.get_mut("ready") {
            ready["state"] = json!("ready");
            *server.state.lock().unwrap_or_else(PoisonError::into_inner) = ready.take();
            continue;
        }
        let id = msg.as_object_mut().and_then(|fields| fields.remove("id")).and_then(|id| id.as_u64());
        let waiter = id.and_then(|id| server.waiting.lock().unwrap_or_else(PoisonError::into_inner).remove(&id));
        if let Some(waiter) = waiter {
            waiter.send(msg).unwrap_or_default();
        }
    }
    if let Some(server) = server.upgrade() {
        *server.state.lock().unwrap_or_else(PoisonError::into_inner) =
            json!({ "state": "failed", "error": "rust-analyzer exited" });
        server.waiting.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

async fn log(repo: String, stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("warmd: {repo} tier 1: {line}");
    }
}
