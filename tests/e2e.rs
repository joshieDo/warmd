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

struct Warmd {
    child: Child,
    url: String,
    root: PathBuf,
}

impl Warmd {
    fn start() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(root.join("demo/src")).unwrap();
        fs::write(
            root.join("demo/Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("demo/src/lib.rs"), LIB).unwrap();
        fs::write(root.join("demo/.gitignore"), "target\n").unwrap();
        sh(
            &root.join("demo"),
            "git init -q -b main && git add -A && git -c user.name=t -c user.email=t@t commit -qm init",
        );
        sh(&root, "git clone -q --bare demo demo.git");
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let repos = format!(
            r#"listen = "127.0.0.1:{port}"
state_dir = "{state}"
poll_secs = 3600
keep_hours = 1

[repos.demo]
url = "file://{root}/demo.git"
jobs = [
  {{ name = "clippy", cmd = "cargo clippy --all-targets --message-format=json -- -D warnings", fix = "cargo clippy --fix --allow-dirty --allow-no-vcs" }},
  {{ name = "slapslop", cmd = 'cargo-slapslop run "$PWD"', on_patch = 'cargo-slapslop run "$PWD" --diff HEAD~1', args = "--all-lints", slapslop = true }},
  {{ name = "warm", cmd = "touch target/warmed", warm_only = true }},
  {{ name = "fmt", cmd = "cargo fmt --all --check", on_patch = "cargo fmt --check {{packages}}" }},
]
"#,
            state = root.join("state").display(),
            root = root.display(),
        );
        fs::write(root.join("repos.toml"), repos).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_warmd")).arg(&root).spawn().unwrap();
        let warmd = Self { child, url: format!("http://127.0.0.1:{port}"), root };
        until(|| warmd.repo()["current"].is_string());
        warmd
    }

    fn repo(&self) -> Value {
        curl("GET", &format!("{}/repos/demo", self.url), None).unwrap_or(Value::Null)
    }

    fn job(&self, path: &str, body: &Value) -> Value {
        let id = curl("POST", &format!("{}/{path}", self.url), Some(body)).unwrap()["id"].as_str().unwrap().to_owned();
        self.wait(&id)
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

    let patch = warmd.diff("printf '\npub fn first(v: &Vec<u32>) -> u32 {\n    v[0]\n}\n' >> src/lib.rs");
    let body = json!({
        "repo": "demo",
        "base": { "ref": "main", "sha": base },
        "patch": patch,
        "full": true,
        "branch": "feat/first",
    });
    let ids = curl("POST", &format!("{}/check", warmd.url), Some(&body)).unwrap();
    let job = warmd.wait(ids["id"].as_str().unwrap());
    let full_job = warmd.wait(ids["full_id"].as_str().unwrap());
    let report = &job["result"];
    assert_eq!(report["outcome"], "checked");
    assert!(report["full"].is_null(), "{report:#}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["ok_precise"], false);
    assert_eq!(report["main_moved"], false);
    assert_eq!(names(&report["jobs"]), ["clippy", "slapslop", "fmt"]);
    assert_eq!(codes(&report["jobs"][0]["new"]), ["clippy::ptr_arg"]);
    let slapslop = report["jobs"][1]["new"].as_array().unwrap();
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
    assert!(full_phases.contains(&"full"), "{full_phases:?}");
    assert!(!full_phases.contains(&"job:clippy"), "{full_phases:?}");

    let mut body = body;
    body["args"] =
        json!({ "clippy": { "add": ["-W", "clippy::pedantic"] }, "slapslop": { "replace": ["--only", "wide_tuple"] } });
    let job = warmd.job("check", &body);
    let report = &job["result"];
    assert_eq!(report["jobs"][0]["custom_args"], true);
    assert!(codes(&report["jobs"][0]["new"]).contains(&"clippy::must_use_candidate"), "{report:#}");
    assert!(codes(&report["jobs"][1]["new"]).iter().all(|code| *code == "wide_tuple"), "{report:#}");

    let first = warmd.diff("printf '\npub fn first(v: &Vec<u32>) -> u32 {\n    v[0]\n}\n' >> src/lib.rs");
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
    assert_eq!(names(&job["result"]["jobs"]), ["clippy"]);
    assert_eq!(job["result"]["ok_precise"], true);
    let comment = warmd.diff("sed -i '1i // note' src/lib.rs");
    let job = warmd.job("check", &json!({ "repo": "demo", "base": { "sha": warmd.head() }, "patch": comment }));
    let fmt = job["result"]["jobs"].as_array().unwrap().iter().find(|job| job["name"] == "fmt").unwrap().clone();
    assert_eq!(fmt["new"], json!([]), "{job:#}");

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

    let stats = curl("GET", &format!("{}/stats", warmd.url), None).unwrap();
    assert!(stats["machine"]["cores"].as_u64() > Some(0), "{stats:#}");
    assert!(!stats["machine"]["cpu_model"].as_str().unwrap().is_empty(), "{stats:#}");
    assert_eq!(stats["repos"][0]["view"]["gens"].as_array().unwrap().len(), 2, "{stats:#}");
    assert!(stats["repos"][0]["view"]["gens"][0]["expires_in_secs"].is_u64(), "{stats:#}");
    assert!(stats["repos"][0]["last_build"]["total_secs"].is_f64(), "{stats:#}");
    assert!(!stats["recent"].as_array().unwrap().is_empty(), "{stats:#}");
    let checks = stats["repos"][0]["checks"].as_array().unwrap();
    let first = checks.iter().find(|check| check["fields"]["branch"] == "feat/first").unwrap();
    assert!(first["fields"]["layer_bytes"].as_u64() > Some(0), "{first:#}");
    assert_eq!(first["fields"]["ran_on"], json!(base));
    assert_eq!((first["fields"]["base"].clone(), first["fields"]["dirty"].clone()), (json!(base), json!(true)));
    until(|| {
        curl("GET", &format!("{}/stats", warmd.url), None).unwrap()["repos"][0]["view"]["gens"][1]["bytes"].is_u64()
    });
    until(|| {
        curl("GET", &format!("{}/stats", warmd.url), None).unwrap()["repos"][0]["view"]["gens"][0]["own_bytes"].is_u64()
    });
    let stats = curl("GET", &format!("{}/stats", warmd.url), None).unwrap();
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
