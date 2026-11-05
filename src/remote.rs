use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, from_str, json, to_string};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    time::{sleep, timeout},
};

use crate::{App, check::Request, metrics::Phase};

const POLL_EVERY: Duration = Duration::from_secs(2);

const CAPACITY_EVERY: Duration = Duration::from_secs(5);

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

const HEADERS: &str = "Content-Type: application/json\r\nConnection: close\r\nContent-Length: ";

#[derive(Debug)]
pub(crate) enum End {
    Done(Value, Vec<Phase>),
    Failed(String, Vec<Phase>),
    Lost(String),
}

#[derive(Debug, Deserialize)]
struct RemoteJob {
    state: String,
    #[serde(default)]
    result: Value,
    error: Option<String>,
    progress: Option<Value>,
    #[serde(default)]
    phases: Vec<Phase>,
}

pub(crate) async fn run(app: &App, addr: &str, req: &Request, lease: Duration, progress: impl Fn(Value)) -> End {
    let id = match call(
        addr,
        "POST",
        match req.kind() {
            "slapslop_baseline" => "/slapslop/baseline",
            "test" => "/test",
            _ => "/check",
        },
        Some(req),
    )
    .await
    {
        Ok((200, reply)) => match from_str::<Value>(&reply) {
            Ok(reply) if reply["id"].is_string() => reply["id"].as_str().unwrap_or_default().to_owned(),
            Ok(reply) => return End::Lost(format!("{addr} answered without a job id: {reply}")),
            Err(err) => return End::Lost(format!("{addr} answered {reply}: {err}")),
        },
        Ok((code, reply)) => return End::Lost(format!("{addr} refused the check: HTTP {code}: {reply}")),
        Err(err) => return End::Lost(format!("sending to {addr}: {err:#}")),
    };
    let mut last = Instant::now();
    loop {
        sleep(POLL_EVERY).await;
        match call(addr, "GET", &format!("/jobs/{id}"), None).await {
            Ok((200, reply)) => {
                last = Instant::now();
                app.balance.heard(addr);
                match from_str::<RemoteJob>(&reply) {
                    Ok(job) if job.state == "done" => {
                        let mut result = job.result;
                        if let Some(fields) = result.as_object_mut() {
                            fields.insert("remote".to_owned(), json!(addr));
                        }
                        return End::Done(result, job.phases);
                    }
                    Ok(job) if job.state == "failed" => {
                        let why = job.error.unwrap_or_else(|| "no error given".to_owned());
                        return End::Failed(format!("{why} (on {addr})"), job.phases);
                    }
                    Ok(RemoteJob { progress: Some(value), .. }) => progress(value),
                    Ok(_) => {}
                    Err(err) => eprintln!("warmd: {addr} job {id}: reading {reply}: {err}"),
                }
            }
            Ok((404, _)) => return End::Lost(format!("{addr} no longer knows job {id} (restarted?)")),
            Ok((code, reply)) => eprintln!("warmd: {addr} job {id}: HTTP {code}: {reply}"),
            Err(err) if last.elapsed() >= lease => {
                return End::Lost(format!("{addr} unreachable for {}s: {err:#}", last.elapsed().as_secs()));
            }
            Err(err) => eprintln!("warmd: {addr} job {id}: {err:#}; asking again"),
        }
    }
}

pub(crate) async fn poll_capacity_forever(app: Arc<App>) -> ! {
    loop {
        for addr in app.balance.remotes() {
            let answer = match call(&addr, "GET", "/capacity", None).await {
                Ok((200, reply)) => free_slots(&reply).with_context(|| format!("{addr}: reading /capacity: {reply}")),
                Ok((code, reply)) => Err(anyhow!("{addr}: /capacity: HTTP {code}: {reply}")),
                Err(err) => Err(err),
            };
            app.balance.polled(&addr, answer);
        }
        sleep(CAPACITY_EVERY).await;
    }
}

fn free_slots(reply: &str) -> anyhow::Result<HashMap<String, usize>> {
    Ok(from_str::<Value>(reply)?["repos"]
        .as_object()
        .context("no repos")?
        .iter()
        .filter(|(_, repo)| repo["built"] == true)
        .filter_map(|(name, repo)| Some((name.clone(), usize::try_from(repo["free"].as_u64()?).ok()?)))
        .collect())
}

async fn call(addr: &str, method: &str, path: &str, body: Option<&Request>) -> anyhow::Result<(u16, String)> {
    let payload = match body {
        Some(req) => to_string(req).context("encoding the request")?,
        None => String::new(),
    };
    parse(
        &String::from_utf8_lossy(
            &timeout(
                HTTP_TIMEOUT,
                exchange(
                    addr,
                    format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{HEADERS}{}\r\n\r\n{payload}", payload.len()),
                ),
            )
            .await
            .with_context(|| format!("{addr}{path}: no answer in {}s", HTTP_TIMEOUT.as_secs()))??,
        ),
        addr,
        path,
    )
}

async fn exchange(addr: &str, request: String) -> anyhow::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr).await.with_context(|| format!("connecting to {addr}"))?;
    stream.write_all(request.as_bytes()).await?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).await?;
    Ok(reply)
}

fn parse(text: &str, addr: &str, path: &str) -> anyhow::Result<(u16, String)> {
    let Some((head, rest)) = text.split_once("\r\n\r\n") else { bail!("{addr}{path}: a reply without headers") };
    ensure!(
        !head.to_ascii_lowercase().contains("transfer-encoding: chunked"),
        "{addr}{path}: a chunked reply, which this client does not read"
    );
    Ok((
        head.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .with_context(|| format!("{addr}{path}: no status in {head}"))?,
        rest.to_owned(),
    ))
}
