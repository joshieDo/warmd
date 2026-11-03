use std::{
    collections::VecDeque,
    io,
    path::Path,
    sync::{Mutex, PoisonError},
    thread::available_parallelism,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Error;
use serde::Serialize;
use tokio::{fs::read_to_string, process::Command, time::interval};

const SAMPLE_EVERY: Duration = Duration::from_secs(10);

const WINDOW: usize = 8640;

const HISTORY_POINTS: usize = 144;

#[derive(Debug, Default)]
pub(crate) struct Machine {
    samples: Mutex<VecDeque<Sample>>,
    per_core: Mutex<Vec<f64>>,
}

impl Machine {
    pub(crate) async fn sample_forever(&self) -> ! {
        let mut ticks = interval(SAMPLE_EVERY);
        let mut last: Option<Vec<CpuTimes>> = None;
        loop {
            ticks.tick().await;
            match (cpu_times().await, memory().await) {
                (Ok(times), Ok((total, used))) => {
                    if let Some(before) = &last {
                        let usage =
                            times.iter().zip(before).map(|(now, then)| now.percent_since(*then)).collect::<Vec<_>>();
                        if let Some((all, cores)) = usage.split_first() {
                            self.push(Sample { at: now_secs(), cpu: *all, mem: percent(used, total) });
                            cores.clone_into(&mut self.per_core.lock().unwrap_or_else(PoisonError::into_inner));
                        }
                    }
                    last = Some(times);
                }
                (Err(err), _) | (_, Err(err)) => eprintln!("warmd: sampling the machine: {err:#}"),
            }
        }
    }

    fn push(&self, sample: Sample) {
        let mut samples = self.samples.lock().unwrap_or_else(PoisonError::into_inner);
        samples.push_back(sample);
        while samples.len() > WINDOW {
            samples.pop_front();
        }
    }

    pub(crate) async fn view(&self, state_dir: &Path) -> MachineView {
        let mut errors = Vec::new();
        let (mem_total, mem_used) = memory().await.unwrap_or_else(|err| {
            errors.push(format!("memory: {err:#}"));
            (0, 0)
        });
        let load = load_average().await.unwrap_or_else(|err| {
            errors.push(format!("load: {err:#}"));
            Vec::new()
        });
        let disks = disks(state_dir).await.unwrap_or_else(|err| {
            errors.push(format!("disks: {err:#}"));
            Vec::new()
        });
        let cpu_model = cpu_model().await.unwrap_or_else(|err| {
            errors.push(format!("cpu model: {err:#}"));
            String::new()
        });
        let cpu_per_core = self.per_core.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let cores = available_parallelism()
            .map_err(Error::from)
            .and_then(|count| Ok(u32::try_from(count.get())?))
            .unwrap_or_else(|err| {
                errors.push(format!("cores: {err:#}"));
                0
            });
        let samples = self.samples.lock().unwrap_or_else(PoisonError::into_inner).iter().copied().collect::<Vec<_>>();
        let step = samples.len().div_ceil(HISTORY_POINTS).max(1);
        MachineView {
            cores,
            cpu_model,
            cpu_per_core,
            load,
            mem_total,
            mem_used,
            disks,
            now: samples.last().copied(),
            peak_cpu: samples.iter().map(|sample| sample.cpu).fold(0.0, f64::max),
            peak_mem: samples.iter().map(|sample| sample.mem).fold(0.0, f64::max),
            history: samples.chunks(step).filter_map(peak_of).collect(),
            errors,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct MachineView {
    cores: u32,
    cpu_model: String,
    cpu_per_core: Vec<f64>,
    load: Vec<f64>,
    mem_total: u64,
    mem_used: u64,
    disks: Vec<Disk>,
    now: Option<Sample>,
    peak_cpu: f64,
    peak_mem: f64,
    history: Vec<Sample>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct Sample {
    at: u64,
    cpu: f64,
    mem: f64,
}

#[derive(Debug, Serialize)]
struct Disk {
    mount: String,
    size: u64,
    used: u64,
}

#[derive(Debug, Clone, Copy)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

impl CpuTimes {
    fn parse(line: &str) -> anyhow::Result<Self> {
        let fields = line.split_whitespace().skip(1).take(8).map(str::parse::<u64>).collect::<Result<Vec<_>, _>>()?;
        let total = fields.iter().sum::<u64>();
        let idle = fields
            .get(3)
            .zip(fields.get(4))
            .map(|(idle, iowait)| idle + iowait)
            .ok_or_else(|| missing("no idle time"))?;
        Ok(Self { busy: total.checked_sub(idle).ok_or_else(|| missing("idle exceeds total"))?, total })
    }

    fn percent_since(self, before: Self) -> f64 {
        percent(self.busy.saturating_sub(before.busy), self.total.saturating_sub(before.total))
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

fn peak_of(chunk: &[Sample]) -> Option<Sample> {
    let last = chunk.last()?;
    Some(Sample {
        at: last.at,
        cpu: chunk.iter().map(|sample| sample.cpu).fold(0.0, f64::max),
        mem: chunk.iter().map(|sample| sample.mem).fold(0.0, f64::max),
    })
}

async fn cpu_times() -> anyhow::Result<Vec<CpuTimes>> {
    let text = read_to_string("/proc/stat").await?;
    text.lines().filter(|line| line.starts_with("cpu")).map(CpuTimes::parse).collect()
}

async fn cpu_model() -> anyhow::Result<String> {
    let text = read_to_string("/proc/cpuinfo").await?;
    let line = text.lines().find(|line| line.starts_with("model name")).ok_or_else(|| missing("no model name"))?;
    Ok(line.split_once(':').map_or(line, |(_, model)| model).trim().to_owned())
}

async fn memory() -> anyhow::Result<(u64, u64)> {
    let text = read_to_string("/proc/meminfo").await?;
    let total = meminfo_field(&text, "MemTotal:")?;
    let available = meminfo_field(&text, "MemAvailable:")?;
    Ok((total, total.checked_sub(available).ok_or_else(|| missing("available exceeds total"))?))
}

fn meminfo_field(text: &str, name: &str) -> anyhow::Result<u64> {
    let rest = text.lines().find_map(|line| line.strip_prefix(name)).ok_or_else(|| missing(name))?;
    Ok(rest.split_whitespace().next().ok_or_else(|| missing(name))?.parse::<u64>()? * 1024)
}

async fn load_average() -> anyhow::Result<Vec<f64>> {
    let text = read_to_string("/proc/loadavg").await?;
    Ok(text.split_whitespace().take(3).map(str::parse).collect::<Result<_, _>>()?)
}

async fn disks(state_dir: &Path) -> anyhow::Result<Vec<Disk>> {
    let out = Command::new("df").args(["-B1", "--output=target,size,used", "/"]).arg(state_dir).output().await?;
    let mut disks: Vec<Disk> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let [mount, size, used] = fields.as_slice() else { continue };
        if !disks.iter().any(|disk| disk.mount == *mount) {
            disks.push(Disk { mount: (*mount).to_owned(), size: size.parse()?, used: used.parse()? });
        }
    }
    Ok(disks)
}

fn missing(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

#[expect(clippy::cast_precision_loss, reason = "a percentage needs no more than f64's 52 bits of precision")]
fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 * 100.0 / whole as f64 }
}
