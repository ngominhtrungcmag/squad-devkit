use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    collections::HashSet,
    io::Read,
    path::Path,
    process::Command,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize)]
pub struct Suite {
    #[serde(default = "default_trials")]
    pub trials: usize,
    pub tasks: Vec<Task>,
}
#[derive(Debug, Deserialize)]
pub struct Task {
    pub id: String,
    pub cmd: String,
    pub timeout_secs: Option<u64>,
    pub fixture_dir: Option<String>,
    pub oracle: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TaskResult {
    pub id: String,
    pub trials: usize,
    pub passed: usize,
    pub pass_at_k: Vec<f64>,
    pub first_pass_rate: f64,
    pub avg_duration_ms: u128,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Report {
    pub trials: usize,
    pub tasks: Vec<TaskResult>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialEvidence {
    pub task_id: String,
    pub attempt: usize,
    pub passed: bool,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub cmd_stdout: String,
    pub cmd_stderr: String,
    pub oracle_stdout: String,
    pub oracle_stderr: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteRun {
    pub report: Report,
    pub evidence: Vec<TrialEvidence>,
}
fn default_trials() -> usize {
    1
}

pub fn pass_at_k(n: usize, c: usize, k: usize) -> f64 {
    if k == 0 || k > n || c > n {
        return 0.0;
    }
    if k > n - c {
        return 1.0;
    }
    let mut fail = 1.0;
    for i in 0..k {
        fail *= (n - c - i) as f64 / (n - i) as f64;
    }
    1.0 - fail
}

pub fn run_fixture_suite(path: &Path, trials: Option<usize>) -> Result<Report> {
    Ok(run_fixture_suite_with_env(path, trials, &[], 0)?.report)
}

pub fn run_fixture_suite_with_env(
    path: &Path,
    trials: Option<usize>,
    env_vars: &[(String, String)],
    max_log_bytes: usize,
) -> Result<SuiteRun> {
    let mut evidence = Vec::new();
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read suite {}", path.display()))?;
    let suite: Suite = if path.extension().and_then(|s| s.to_str()) == Some("json") {
        serde_json::from_str(&content)?
    } else {
        serde_yaml::from_str(&content)?
    };
    let n = trials.unwrap_or(suite.trials);
    if n == 0 || suite.tasks.is_empty() {
        bail!("suite must have tasks and trials must be positive");
    }
    check_duplicate_ids(suite.tasks.iter().map(|task| task.id.as_str()))?;
    let mut results = Vec::new();
    for task in suite.tasks {
        let mut passed = 0;
        let mut first = false;
        let mut durations = 0u128;
        for attempt in 0..n {
            let start = Instant::now();
            let trial_dir = TrialDir::new()?;
            if let Some(fixture) = &task.fixture_dir {
                copy_fixture(Path::new(fixture), &trial_dir.0)?;
            }
            let (status, mut timed_out, cmd_stdout, cmd_stderr) = run_command(
                &task.cmd,
                &trial_dir.0,
                task.timeout_secs,
                env_vars,
                max_log_bytes,
            )
            .with_context(|| format!("run task {}", task.id))?;
            let (mut oracle_stdout, mut oracle_stderr) = (String::new(), String::new());
            let trial_passed = if timed_out {
                false
            } else if let Some(oracle) = &task.oracle {
                let (status, timeout, stdout, stderr) = run_command(
                    oracle,
                    &trial_dir.0,
                    task.timeout_secs,
                    env_vars,
                    max_log_bytes,
                )
                .with_context(|| format!("run oracle {}", task.id))?;
                timed_out = timeout;
                oracle_stdout = stdout;
                oracle_stderr = stderr;
                !timeout && status.success()
            } else {
                status.success()
            };
            let duration_ms = start.elapsed().as_millis();
            durations += duration_ms;
            if max_log_bytes > 0 {
                evidence.push(TrialEvidence {
                    task_id: task.id.clone(),
                    attempt: attempt + 1,
                    passed: trial_passed,
                    timed_out,
                    duration_ms,
                    cmd_stdout,
                    cmd_stderr,
                    oracle_stdout,
                    oracle_stderr,
                });
            }
            if trial_passed {
                passed += 1;
                if attempt == 0 {
                    first = true;
                }
            }
        }
        results.push(TaskResult {
            id: task.id,
            trials: n,
            passed,
            pass_at_k: (1..=n).map(|k| pass_at_k(n, passed, k)).collect(),
            first_pass_rate: if first { 1.0 } else { 0.0 },
            avg_duration_ms: durations / n as u128,
        });
    }
    Ok(SuiteRun {
        report: Report {
            trials: n,
            tasks: results,
        },
        evidence,
    })
}

fn run_command(
    command: &str,
    directory: &Path,
    timeout: Option<u64>,
    env_vars: &[(String, String)],
    max_log_bytes: usize,
) -> Result<(std::process::ExitStatus, bool, String, String)> {
    let logs = if max_log_bytes > 0 {
        Some(TrialDir::new()?)
    } else {
        None
    };
    let mut child_cmd = Command::new("sh");
    child_cmd
        .arg("-c")
        .arg(command)
        .current_dir(directory)
        .envs(env_vars.iter().map(|(k, v)| (k, v)));
    if let Some(logs) = &logs {
        child_cmd.stdout(Stdio::from(std::fs::File::create(logs.0.join("stdout"))?));
        child_cmd.stderr(Stdio::from(std::fs::File::create(logs.0.join("stderr"))?));
    }
    #[cfg(unix)]
    child_cmd.process_group(0);
    let mut child = child_cmd.spawn()?;
    let deadline = timeout.map(|t| Instant::now() + Duration::from_secs(t));
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .args(["-KILL", &format!("-{}", child.id())])
                    .stderr(Stdio::null())
                    .status();
            }
            break (status, false);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .args(["-KILL", &format!("-{}", child.id())])
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let read = |name: &str| -> Result<String> {
        let mut bytes = Vec::new();
        if let Some(logs) = &logs {
            std::fs::File::open(logs.0.join(name))?
                .take(max_log_bytes as u64)
                .read_to_end(&mut bytes)?;
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    Ok((status, timed_out, read("stdout")?, read("stderr")?))
}

struct TrialDir(std::path::PathBuf);

impl TrialDir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("squad-eval-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).context("create trial directory")?;
        Ok(Self(path))
    }
}

impl Drop for TrialDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_fixture(source: &Path, destination: &Path) -> Result<()> {
    for entry in
        std::fs::read_dir(source).with_context(|| format!("read fixture {}", source.display()))?
    {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            std::fs::create_dir(&target)?;
            copy_fixture(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            bail!(
                "fixture contains unsupported entry: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

pub fn check_baseline(report: &Report, baseline: &Report) -> Result<()> {
    check_duplicate_ids(report.tasks.iter().map(|task| task.id.as_str()))?;
    check_duplicate_ids(baseline.tasks.iter().map(|task| task.id.as_str()))?;
    for task in &baseline.tasks {
        if !report.tasks.iter().any(|candidate| candidate.id == task.id) {
            bail!("candidate report missing baseline task: {}", task.id);
        }
    }
    for task in &report.tasks {
        let old = baseline
            .tasks
            .iter()
            .find(|x| x.id == task.id)
            .context("baseline missing task")?;
        if task.pass_at_k.first().copied().unwrap_or(0.0)
            < old.pass_at_k.first().copied().unwrap_or(0.0)
        {
            bail!(
                "regression for {}: pass@1 {:.3} -> {:.3}",
                task.id,
                old.pass_at_k.first().copied().unwrap_or(0.0),
                task.pass_at_k.first().copied().unwrap_or(0.0)
            );
        }
    }
    Ok(())
}

fn check_duplicate_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            bail!("duplicate task ID: {id}");
        }
    }
    Ok(())
}
