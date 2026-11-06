use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread::sleep,
    time::{Duration, Instant},
};

use serde_json::{Value, from_slice, from_str, json};

const TIMEOUT: Duration = Duration::from_secs(600);

const LIB: &str = "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n";

const JOBS: &str = r#"  { name = "clippy", cmd = "cargo clippy --all-targets --message-format=json -- -D warnings", fix = "cargo clippy --fix --allow-dirty --allow-no-vcs" },
  { name = "bins", cmd = "cargo clippy --bins --message-format=json -- -D warnings", bins_only = true },
  { name = "slapslop", cmd = 'cargo-slapslop run "$PWD" --diff HEAD~1', on_main = 'cargo-slapslop run "$PWD"', args = "--all-lints", slapslop = true },
  { name = "fmt", cmd = "cargo fmt --all --check", on_patch = "cargo fmt --check {packages}" },
"#;

const CLIPPY: &str = r#"  { name = "clippy", cmd = "cargo clippy --all-targets --message-format=json -- -D warnings" },
"#;

const STUCK_CLIPPY: &str = r#"  { name = "clippy", cmd = "cargo clippy --all-targets --message-format=json -- -D warnings", on_patch = "sleep 600" },
"#;

const TESTS: &str = r#"  { name = "fmt", cmd = "cargo fmt --check" },
  { name = "test", cmd = "cargo test -p {package} -- {tests}", on_main = "cargo test --no-run", test = true },
"#;

const TWO_TESTS: &str = "printf '\n#[test]\nfn adds() {\n    assert_eq!(add(1, 2), 3);\n}\n\n#[test]\nfn breaks() {\n    assert_eq!(add(1, 2), 4);\n}\n' >> src/lib.rs";

const BAD_TYPE: &str = "printf '\npub fn bad() -> u32 {\n    \"x\"\n}\n' >> src/lib.rs";

const USES_TWO: &str = "printf '\npub fn uses_two() -> bool {\n    two()\n}\n' >> src/lib.rs";

const FIRST: &str = "printf '\npub fn first(v: &Vec<u32>) -> u32 {\n    v[0]\n}\n' >> src/lib.rs";

const MAIN_RS: &str =
    "printf 'fn main() {\\n    println!(\"{}\", demo::add(2, 3));\\n}\\n' > src/main.rs && git add -N src/main.rs";

struct Warmd {
    child: Child,
    url: String,
    root: PathBuf,
}

impl Warmd {
    fn start() -> Self {
        let root = demo_root("e2e");
        Self::serve(root.clone(), &root, "", JOBS, &[])
    }

    fn serve(dir: PathBuf, origin: &Path, top: &str, jobs: &str, env: &[(&str, &str)]) -> Self {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        Self::serve_with(dir, origin, port, top, jobs, env)
    }

    fn serve_on(dir: PathBuf, origin: &Path, port: u16, top: &str, jobs: &str) -> Self {
        Self::serve_with(dir, origin, port, top, jobs, &[])
    }

    fn serve_with(dir: PathBuf, origin: &Path, port: u16, top: &str, jobs: &str, env: &[(&str, &str)]) -> Self {
        fs::create_dir_all(&dir).unwrap();
        let repos = format!(
            "listen = \"127.0.0.1:{port}\"\nstate_dir = \"{state}\"\npoll_secs = 3600\nkeep_hours = 1\n{top}\n[repos.demo]\nurl = \"file://{origin}/demo.git\"\njobs = [\n{jobs}]\n",
            state = dir.join("state").display(),
            origin = origin.display(),
        );
        fs::write(dir.join("repos.toml"), repos).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_warmd")).arg(&dir).envs(env.iter().copied()).spawn().unwrap();
        let warmd = Self { child, url: format!("http://127.0.0.1:{port}"), root: origin.to_owned() };
        until(|| warmd.repo()["current"].is_string());
        warmd
    }

    fn get(&self, path: &str) -> Value {
        curl("GET", &format!("{}{path}", self.url), None).unwrap_or(Value::Null)
    }

    fn repo(&self) -> Value {
        self.get("/repos/demo")
    }

    fn job(&self, path: &str, body: &Value) -> Value {
        self.job_with_id(path, body).1
    }

    fn job_with_id(&self, path: &str, body: &Value) -> (String, Value) {
        let id = curl("POST", &format!("{}/{path}", self.url), Some(body)).unwrap()["id"].as_str().unwrap().to_owned();
        let job = self.wait(&id);
        (id, job)
    }

    fn wait(&self, id: &str) -> Value {
        let mut job = Value::Null;
        until(|| {
            job = curl("GET", &format!("{}/jobs/{id}", self.url), None).unwrap();
            job["state"] == "done" || job["state"] == "failed"
        });
        assert_eq!(job["state"], "done", "{job:#}");
        job
    }

    fn head(&self) -> String {
        sh(&self.root.join("demo"), "git rev-parse HEAD").trim().to_owned()
    }

    fn diff(&self, edit: &str) -> String {
        let demo = self.root.join("demo");
        sh(&demo, edit);
        let diff = sh(&demo, "git diff");
        sh(&demo, "git checkout -q .");
        diff
    }
}

impl Drop for Warmd {
    fn drop(&mut self) {
        if let Err(err) = self.child.kill() {
            eprintln!("killing warmd: {err}");
        }
    }
}

fn demo_root(tag: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{tag}-{}", std::process::id()));
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(root.join("demo/src")).unwrap();
    fs::write(root.join("demo/Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
        .unwrap();
    fs::write(root.join("demo/src/lib.rs"), LIB).unwrap();
    fs::write(root.join("demo/.gitignore"), "target\n").unwrap();
    sh(&root.join("demo"), "git init -q -b main && git add -A && git -c user.name=t -c user.email=t@t commit -qm init");
    sh(&root, "git clone -q --bare demo demo.git");
    root
}

fn until(mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < TIMEOUT, "timed out");
        sleep(Duration::from_millis(500));
    }
}

fn sh(dir: &Path, cmd: &str) -> String {
    let out = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn curl(method: &str, url: &str, body: Option<&Value>) -> Option<Value> {
    let mut cmd = Command::new("curl");
    cmd.args(["-sf", "-X", method, url, "-H", "content-type: application/json"]);
    if let Some(body) = body {
        cmd.args(["-d", &body.to_string()]);
    }
    let out = cmd.output().unwrap();
    out.status.success().then(|| from_slice(&out.stdout).unwrap())
}

fn names(jobs: &Value) -> Vec<&str> {
    jobs.as_array().unwrap().iter().map(|job| job["name"].as_str().unwrap()).collect()
}

fn codes(diags: &Value) -> Vec<&str> {
    diags.as_array().unwrap().iter().map(|diag| diag["code"].as_str().unwrap_or_default()).collect()
}

#[test]
fn checks_patches_against_warm_generations() {
    let warmd = Warmd::start();
    let base = warmd.head();
    let hits = curl("GET", &format!("{}/repos/demo/hits", warmd.url), None).unwrap();
    assert_eq!(hits["sha"], base);
    assert!(hits["jobs"]["slapslop"].is_array());

    let patch = warmd.diff(FIRST);
    let body = json!({
        "repo": "demo",
        "base": { "ref": "main", "sha": base },
        "patch": patch,
        "full": true,
        "branch": "feat/first",
    });
    let ids = curl("POST", &format!("{}/check", warmd.url), Some(&body)).unwrap();
    let live = warmd.get("/stats")["live"].clone();
    assert!(live.as_array().unwrap().iter().any(|job| job["kind"] == "full"), "{live:#}");
    assert!(live.as_array().unwrap().iter().all(|job| job["request"]["patch_lines"].as_u64() > Some(0)), "{live:#}");
    let job = warmd.wait(ids["id"].as_str().unwrap());
    let full_job = warmd.wait(ids["full_id"].as_str().unwrap());
    let report = &job["result"];
    assert_eq!(report["outcome"], "checked");
    assert!(report["full"].is_null(), "{report:#}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["ok_precise"], false);
    assert_eq!(report["main_moved"], false);
    // Lanes run side by side, so jobs come in the order they finished.
    let mut ran = names(&report["jobs"]);
    ran.sort_unstable();
    assert_eq!(ran, ["clippy", "fmt", "slapslop"]);
    let lanes = job["phases"].as_array().unwrap().iter().filter_map(|phase| phase["lane"].as_str()).collect::<Vec<_>>();
    assert!(["clippy", "fmt", "slapslop"].iter().all(|lane| lanes.contains(lane)), "{lanes:?}");
    let by_name = |name: &str| report["jobs"].as_array().unwrap().iter().find(|job| job["name"] == name).unwrap();
    assert_eq!(codes(&by_name("clippy")["new"]), ["clippy::ptr_arg"], "{job:#}");
    let slapslop = by_name("slapslop")["new"].as_array().unwrap();
    assert!(slapslop.iter().all(|hit| hit["line"].as_u64() >= Some(4)), "{slapslop:#?}");
    assert!(slapslop.iter().any(|hit| hit["hazy"] == true), "{slapslop:#?}");
    let full = &full_job["result"]["full"];
    assert!(full["notice"].as_str().unwrap().contains("Only the CI jobs"), "{full:#}");
    let hits = full["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "{full:#}");
    assert!(hits.iter().all(|hit| hit["line"].as_u64() >= Some(4)), "{full:#}");
    let phases =
        job["phases"].as_array().unwrap().iter().map(|phase| phase["name"].as_str().unwrap()).collect::<Vec<_>>();
    assert!(phases.contains(&"job:clippy"), "{phases:?}");
    let full_phases =
        full_job["phases"].as_array().unwrap().iter().map(|phase| phase["name"].as_str().unwrap()).collect::<Vec<_>>();
    assert!(full_phases.contains(&"full:slapslop"), "{full_phases:?}");
    assert!(full_phases.contains(&"full:clippy"), "{full_phases:?}");
    assert!(!full_phases.contains(&"job:clippy"), "{full_phases:?}");

    let mut body = body;
    body["args"] =
        json!({ "clippy": { "add": ["-W", "clippy::pedantic"] }, "slapslop": { "replace": ["--only", "wide_tuple"] } });
    let job = warmd.job("check", &body);
    let report = &job["result"];
    assert_eq!(report["jobs"][0]["custom_args"], true);
    let mut skipped = body.clone();
    skipped["skip"] = json!(["slapslop"]);
    skipped["args"] = json!({});
    let job = warmd.job("check", &skipped);
    assert!(!names(&job["result"]["jobs"]).contains(&"slapslop"), "{job:#}");
    assert_eq!(job["result"]["not_run"], json!(["slapslop"]), "{job:#}");
    let mut only = skipped;
    only["skip"] = json!([]);
    only["only"] = json!(["fmt"]);
    let job = warmd.job("check", &only);
    assert_eq!(names(&job["result"]["jobs"]), ["fmt"], "{job:#}");
    assert_eq!(job["result"]["not_run"], json!(["clippy", "bins", "slapslop"]), "{job:#}");
    only["only"] = json!(["clippy"]);
    let job = warmd.job("check", &only);
    assert_eq!(names(&job["result"]["jobs"]), ["clippy"], "{job:#}");
    assert!(codes(&report["jobs"][0]["new"]).contains(&"clippy::must_use_candidate"), "{report:#}");
    assert!(codes(&report["jobs"][1]["new"]).iter().all(|code| *code == "wide_tuple"), "{report:#}");

    let first = warmd.diff(FIRST);
    let session =
        |patch: &str| json!({ "repo": "demo", "base": { "sha": base }, "patch": patch, "session": "feat/first" });
    let job = warmd.job("check", &session(&first));
    assert_eq!(job["result"]["session_reused"], false, "{job:#}");
    assert_eq!(codes(&job["result"]["jobs"][0]["new"]), ["clippy::ptr_arg"]);
    let fixed = warmd.diff("printf '\npub fn first(v: &[u32]) -> u32 {\n    v[0]\n}\n' >> src/lib.rs");
    let job = warmd.job("check", &session(&fixed));
    assert_eq!(job["result"]["session_reused"], true, "{job:#}");
    assert!(job["result"]["jobs"][0]["new"].as_array().unwrap().is_empty(), "{job:#}");
    let job = warmd.job("check", &session(""));
    assert_eq!(job["result"]["session_reused"], true, "{job:#}");
    assert_eq!(job["result"]["ok"], true, "{job:#}");
    let job = warmd.job("check", &session(&first));
    assert_eq!(codes(&job["result"]["jobs"][0]["new"]), ["clippy::ptr_arg"]);

    let patch = warmd.diff("printf '\npub fn none(v: &[u32]) -> bool {\n    v.len() == 0\n}\n' >> src/lib.rs");
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": base }, "patch": patch, "fix": true }));
    assert!(job["result"]["fix_patch"].as_str().unwrap().contains("is_empty()"), "{job:#}");

    let secret = format!("warmd-e2e-{}", std::process::id());
    let secret_file = PathBuf::from(env!("HOME")).join(format!(".{secret}"));
    fs::write(&secret_file, &secret).unwrap();
    let reader = format!(
        "fn main() {{\n    let home = std::env::var(\"HOME\").unwrap();\n    if let Ok(found) = std::fs::read_to_string(format!(\"{{home}}/.{secret}\")) {{\n        panic!(\"leaked {{found}}\");\n    }}\n}}\n"
    );
    let demo = warmd.root.join("demo");
    fs::write(demo.join("build.rs"), reader).unwrap();
    let leak = sh(&demo, "git add -N build.rs && git diff");
    sh(&demo, "git rm -q --cached build.rs && rm build.rs");
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": base }, "patch": leak }));
    fs::remove_file(&secret_file).unwrap();
    assert!(!job.to_string().contains(&format!("leaked {secret}")), "{job:#}");

    let job = warmd.job("slapslop/baseline", &json!({ "repo": "demo", "base": { "sha": base } }));
    assert!(job["result"]["baseline"].as_str().unwrap().contains("[[accept]]"), "{job:#}");

    let conflicting = warmd.diff("sed -i 's/a + b/a * b/' src/lib.rs");
    sh(
        &warmd.root.join("demo"),
        "sed -i 's/a + b/b + a/' src/lib.rs && printf 'pub fn ugly()->u32{1}\\n' >> src/lib.rs && git -c user.name=t -c user.email=t@t commit -qam swap",
    );
    sh(&warmd.root.join("demo"), "git push -q ../demo.git main");
    // A check on main's new head, before the poll built it, builds it first and runs on it (no rebase).
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": warmd.head() }, "patch": "" }));
    assert_eq!(
        (job["result"]["ran_on"].clone(), job["result"]["rebased_from"].clone()),
        (json!(warmd.head()), json!(null)),
        "{job:#}"
    );
    let job = warmd.job("repos/demo/refresh", &json!({}));
    assert_eq!(job["result"]["current"], warmd.head());
    assert_eq!(job["result"]["gens"].as_array().unwrap().len(), 2);
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": base }, "patch": conflicting }));
    assert_eq!((job["result"]["main_moved"].clone(), job["result"]["ran_on"].clone()), (json!(true), json!(base)));
    let gone = "0".repeat(40);
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": gone }, "patch": conflicting }));
    assert_eq!(job["result"], json!({ "outcome": "rebase_needed", "files": ["src/lib.rs"] }));
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": gone }, "patch": "" }));
    assert_eq!(job["result"]["rebased_from"], gone);
    assert_eq!(names(&job["result"]["jobs"]), ["clippy", "slapslop"]);
    assert_eq!(job["result"]["ok_precise"], true);
    assert!(job["result"]["jobs"][0]["rebuilt"].is_u64(), "{job:#}");
    assert!(job["result"]["jobs"][0]["cpu_secs"].as_f64() > Some(0.0), "{job:#}");
    let comment = warmd.diff("sed -i '1i // note' src/lib.rs");
    let stale = json!({ "repo": "demo", "base": { "sha": gone }, "patch": comment, "session": "stale-base" });
    warmd.job("check", &stale);
    let job = warmd.job("check", &stale);
    assert_eq!(
        (job["result"]["rebased_from"].clone(), job["result"]["session_reused"].clone()),
        (json!(gone), json!(true)),
        "{job:#}"
    );
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": warmd.head() }, "patch": comment }));
    let fmt = job["result"]["jobs"].as_array().unwrap().iter().find(|job| job["name"] == "fmt").unwrap().clone();
    assert_eq!(fmt["new"], json!([]), "{job:#}");
    let ugly = warmd.diff(
        "printf 'mod m;\\n' >> src/lib.rs && printf 'pub fn ugly()->u32{1}\\n' > src/m.rs && git add -N src/m.rs",
    );
    sh(&warmd.root.join("demo"), "git rm -qf src/m.rs");
    let job =
        warmd.job("check", &json!({ "repo": "demo", "base": { "sha": warmd.head() }, "patch": ugly, "only": ["fmt"] }));
    let rendered = job["result"]["jobs"][0]["new"][0]["rendered"].as_str().unwrap_or_default().to_owned();
    assert!(rendered.starts_with("Diff in src/m.rs:"), "{job:#}");

    let repos = fs::read_to_string(warmd.root.join("repos.toml")).unwrap();
    fs::write(
        warmd.root.join("repos.toml"),
        repos.replacen("jobs = [", "jobs = [\n  { name = \"extra\", cmd = \"true\" },", 1),
    )
    .unwrap();
    sh(&warmd.root, &format!("kill -HUP {}", warmd.child.id()));
    sleep(Duration::from_secs(1));
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": warmd.head() }, "patch": comment }));
    assert_eq!(job["result"]["jobs"][0]["name"], "extra", "{job:#}");

    // A job that fails on main itself is skipped by checks and reported, without failing main's build.
    let repos = fs::read_to_string(warmd.root.join("repos.toml")).unwrap();
    fs::write(
        warmd.root.join("repos.toml"),
        repos.replacen("jobs = [", "jobs = [\n  { name = 'broken', cmd = 'echo nope >&2; exit 3' },", 1),
    )
    .unwrap();
    sh(&warmd.root, &format!("kill -HUP {}", warmd.child.id()));
    sleep(Duration::from_secs(1));
    sh(
        &warmd.root.join("demo"),
        "git -c user.name=t -c user.email=t@t commit -q --allow-empty -m next && git push -q ../demo.git main",
    );
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": warmd.head() }, "patch": comment }));
    assert_eq!(job["result"]["ran_on"], warmd.head(), "{job:#}");
    assert_eq!(job["result"]["broken_on_main"], json!(["broken"]), "{job:#}");
    assert!(!names(&job["result"]["jobs"]).contains(&"broken"), "{job:#}");
    assert_eq!(job["result"]["ok"], true, "{job:#}");

    let stats = warmd.get("/stats");
    assert!(stats["machine"]["cores"].as_u64() > Some(0), "{stats:#}");
    assert!(!stats["machine"]["cpu_model"].as_str().unwrap().is_empty(), "{stats:#}");
    assert_eq!(stats["repos"][0]["view"]["gens"].as_array().unwrap().len(), 3, "{stats:#}");
    assert!(stats["repos"][0]["view"]["gens"][0]["expires_in_secs"].is_u64(), "{stats:#}");
    assert!(stats["repos"][0]["last_build"]["total_secs"].is_f64(), "{stats:#}");
    assert!(!stats["recent"].as_array().unwrap().is_empty(), "{stats:#}");
    assert!(stats["recent"].as_array().unwrap().iter().any(|job| job["kind"] == "build"), "{stats:#}");
    assert_eq!(stats["live"], json!([]), "{stats:#}");
    let checks = stats["repos"][0]["checks"].as_array().unwrap();
    let first = checks.iter().find(|check| check["fields"]["branch"] == "feat/first").unwrap();
    assert!(first["fields"]["layer_bytes"].as_u64() > Some(0), "{first:#}");
    assert!(first["fields"]["request"]["patch_lines"].as_u64() > Some(0), "{first:#}");
    let clippy = checks
        .iter()
        .flat_map(|check| check["phases"].as_array().unwrap())
        .find(|phase| phase["name"] == "job:clippy")
        .unwrap();
    assert!(clippy["commands"][0].as_str().unwrap().contains("cargo clippy"), "{clippy:#}");
    assert!(clippy["start_secs"].as_f64() > Some(0.0), "{clippy:#}");
    assert_eq!(first["fields"]["ran_on"], json!(base));
    assert_eq!((first["fields"]["base"].clone(), first["fields"]["dirty"].clone()), (json!(base), json!(true)));
    until(|| warmd.get("/stats")["repos"][0]["view"]["gens"][1]["bytes"].is_u64());
    until(|| warmd.get("/stats")["repos"][0]["view"]["gens"][0]["own_bytes"].is_u64());
    let stats = warmd.get("/stats");
    let parts = stats["repos"][0]["view"]["gens"][1]["parts"].as_array().unwrap();
    let jobs_of = |bucket: &str| {
        parts
            .iter()
            .find(|part| part["jobs"].as_array().unwrap().iter().any(|job| job == bucket))
            .map(|part| part["jobs"].clone())
    };
    assert!(jobs_of("clippy").is_some(), "{parts:#?}");
    assert!(jobs_of("slapslop").is_some(), "{parts:#?}");
    assert!(jobs_of("fmt").is_some(), "{parts:#?}");
    assert_eq!(stats["repos"][0]["url"], json!(format!("file://{}/demo", warmd.root.display())));
    assert!(sh(&warmd.root, &format!("curl -sf {}/", warmd.url)).contains("/stats"));
    let metrics = fs::read_to_string(warmd.root.join("state/demo/metrics.jsonl")).unwrap();
    let kinds = metrics.lines().map(|line| from_str::<Value>(line).unwrap()["kind"].clone()).collect::<Vec<_>>();
    assert!(kinds.contains(&json!("build")), "{kinds:?}");
    assert!(kinds.contains(&json!("check")), "{kinds:?}");
}

#[test]
fn spreads_checks_to_remotes_and_requeues_when_one_drops() {
    let root = demo_root("lb");
    let stuck = Warmd::serve(root.join("stuck"), &root, "", STUCK_CLIPPY, &[]);
    let fresh_port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let stuck_addr = stuck.url.trim_start_matches("http://").to_owned();
    let fresh_addr = format!("127.0.0.1:{fresh_port}");
    let lead = Warmd::serve(root.clone(), &root, "jobs_per_repo = 0\nlease_secs = 3", CLIPPY, &[]);
    let remotes = format!("{}/remotes", lead.url);
    assert!(
        curl("POST", &remotes, Some(&json!({ "addr": "no-port" }))).is_none(),
        "an address without a port is refused"
    );
    for addr in [&stuck_addr, &fresh_addr] {
        assert_eq!(curl("POST", &remotes, Some(&json!({ "addr": addr }))).unwrap()["added"], true);
    }
    let patch = lead.diff(FIRST);
    let body = json!({ "repo": "demo", "base": { "sha": lead.head() }, "patch": patch, "branch": "feature", "session": "feature" });
    let id = curl("POST", &format!("{}/check", lead.url), Some(&body)).unwrap()["id"].as_str().unwrap().to_owned();
    until(|| {
        lead.get("/stats")["live"].as_array().is_some_and(|live| live.iter().any(|job| job["remote"] == stuck_addr))
    });
    drop(stuck);

    let fresh = Warmd::serve_on(root.join("fresh"), &root, fresh_port, "", CLIPPY);
    let job = lead.wait(&id);
    assert_eq!(job["result"]["remote"], fresh_addr, "{job:#}");
    assert_eq!(names(&job["result"]["jobs"]), ["clippy"]);
    assert!(codes(&job["result"]["jobs"][0]["new"]).contains(&"clippy::ptr_arg"), "{job:#}");
    let seen = lead.get("/stats")["balance"]["remotes"].clone();
    let gone = seen.as_array().unwrap().iter().find(|remote| remote["addr"] == stuck_addr).unwrap();
    assert_eq!(gone["up"], false, "a remote that lost a job counts as down: {seen:#}");

    let again = lead.job("check", &body);
    let removed = curl("DELETE", &format!("{remotes}/{stuck_addr}"), None).unwrap();
    assert_eq!(removed["removed"], true, "{removed:#}");
    assert_eq!(lead.get("/remotes")["remotes"].as_array().map(Vec::len), Some(1));
    assert_eq!(again["result"]["remote"], fresh_addr, "{again:#}");
    assert_eq!(again["result"]["session_reused"], true, "the branch went back to its warm session: {again:#}");
    drop(fresh);
}

#[test]
fn runs_only_the_named_tests_of_a_package() {
    let root = demo_root("tests");
    let warmd = Warmd::serve(root.clone(), &root, "", TESTS, &[]);
    let patch = warmd.diff(TWO_TESTS);
    let base = json!({ "sha": warmd.head() });
    let check = warmd.job("check", &json!({ "repo": "demo", "base": base, "patch": patch }));
    assert_eq!(names(&check["result"]["jobs"]), ["fmt"], "a check never runs tests: {check:#}");

    let test = format!("{}/test", warmd.url);
    let whole = json!({ "repo": "demo", "base": base, "patch": patch, "tests": { "package": "demo", "names": [] } });
    assert!(curl("POST", &test, Some(&whole)).is_none(), "a run without test names is refused");
    let via_check =
        json!({ "repo": "demo", "base": base, "patch": patch, "tests": { "package": "demo", "names": ["adds"] } });
    assert!(curl("POST", &format!("{}/check", warmd.url), Some(&via_check)).is_none(), "tests only go through /test");

    let job = warmd.job("test", &via_check);
    let ran = &job["result"]["jobs"];
    assert_eq!(names(ran), ["test"], "{job:#}");
    assert_eq!(ran[0]["exit"], 0, "{job:#}");
    let output = ran[0]["output"].as_str().unwrap();
    assert!(output.contains("test adds ... ok"), "{output}");
    assert!(!output.contains("breaks"), "only the named test ran: {output}");
}

fn tier1(warmd: &Warmd, edit: &str) -> Value {
    let body = json!({ "repo": "demo", "base": { "ref": "main", "sha": warmd.head() }, "patch": warmd.diff(edit) });
    let id = curl("POST", &format!("{}/check", warmd.url), Some(&body)).unwrap()["id"].as_str().unwrap().to_owned();
    let mut job = Value::Null;
    until(|| {
        job = warmd.get(&format!("/jobs/{id}"));
        !job["tier1"].is_null()
    });
    job
}

#[test]
fn tier1_answers_before_the_jobs_and_follows_main() {
    let root = demo_root("tier1");
    let channel =
        include_str!("../rust-toolchain.toml").lines().find_map(|line| line.strip_prefix("channel = ")).unwrap();
    let top = format!(
        "[repos.demo.ra]\nbuild = \"cargo check --all-targets --message-format=json\"\ntoolchain = {channel}\n"
    );
    let warmd = Warmd::serve(root.clone(), &root, &top, STUCK_CLIPPY, &[]);
    until(|| warmd.repo()["tier1"]["state"] != "loading");
    assert_eq!(warmd.repo()["tier1"]["state"], "ready", "{:#}", warmd.repo());

    let job = tier1(&warmd, BAD_TYPE);
    assert_eq!(job["state"], "running", "{job:#}");
    let found = &job["tier1"]["diagnostics"];
    assert_eq!(codes(found), ["E0308"], "{job:#}");
    assert_eq!(found[0]["file"], "src/lib.rs");
    assert_eq!(found[0]["line"], 6);
    assert_eq!(tier1(&warmd, FIRST)["tier1"]["diagnostics"], json!([]));

    sh(
        &root.join("demo"),
        "printf '\npub fn two() -> u32 {\n    2\n}\n' >> src/lib.rs && git -c user.name=t -c user.email=t@t commit -qam two && git push -q ../demo.git main",
    );
    let head = warmd.head();
    curl("POST", &format!("{}/repos/demo/refresh", warmd.url), None).unwrap();
    until(|| warmd.repo()["tier1"]["sha"] == head.as_str());
    assert_eq!(warmd.repo()["tier1"]["state"], "ready");
    let job = tier1(&warmd, USES_TWO);
    assert_eq!(codes(&job["tier1"]["diagnostics"]), ["E0308"], "{job:#}");
}

#[test]
fn builds_the_binary_of_a_patch_in_debug_or_release() {
    let root = demo_root("build");
    let warmd = Warmd::serve(root.clone(), &root, "", TESTS, &[]);
    let patch = warmd.diff(MAIN_RS);
    let base = json!({ "sha": warmd.head() });
    let build = format!("{}/build", warmd.url);
    let bad = json!({ "repo": "demo", "base": base, "patch": patch, "binary": { "package": "--all" } });
    assert!(curl("POST", &build, Some(&bad)).is_none(), "a flag is not a package");
    let via_check = json!({ "repo": "demo", "base": base, "patch": patch, "binary": { "package": "demo" } });
    assert!(curl("POST", &format!("{}/check", warmd.url), Some(&via_check)).is_none(), "builds only go through /build");

    for (release, profile) in [(false, "debug"), (true, "release")] {
        let body = json!({ "repo": "demo", "base": base, "patch": patch, "binary": { "package": "demo", "release": release } });
        let (id, job) = warmd.job_with_id("build", &body);
        let built = &job["result"]["binary"];
        assert_eq!(job["result"]["ok"], true, "{job:#}");
        assert_eq!((built["name"].as_str(), built["profile"].as_str()), (Some("demo"), Some(profile)), "{job:#}");
        let file = root.join(format!("demo-{profile}"));
        let url = format!("{}/jobs/{id}/binary", warmd.url);
        sh(&root, &format!("curl -fsS -o {} {url} && chmod +x {}", file.display(), file.display()));
        assert_eq!(sh(&root, &file.display().to_string()), "5\n", "the {profile} binary runs the patch");
    }

    let broken =
        warmd.diff("printf 'fn main() {\\n    let _x: u32 = \"no\";\\n}\\n' > src/main.rs && git add -N src/main.rs");
    let job =
        warmd.job("build", &json!({ "repo": "demo", "base": base, "patch": broken, "binary": { "package": "demo" } }));
    assert_eq!(job["result"]["ok"], false, "{job:#}");
    assert!(job["result"]["output"].as_str().is_some_and(|out| out.contains("mismatched types")), "{job:#}");
    let units = root.join("state/rustc/target/.warmd-rustc/v1/units");
    assert!(fs::read_dir(&units).is_ok_and(|mut found| found.next().is_some()), "rustc's outputs went to the cache");
}
