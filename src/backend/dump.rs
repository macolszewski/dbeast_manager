//! PostgreSQL client tools as background transfers: the child process runs
//! with libpq environment variables, its output streams into the Run log and
//! Stop kills it. A dump goes to a hidden file that replaces the target only
//! when pg_dump succeeds.

use super::transfer::{LogLine, PROGRESS_INTERVAL, Progress, TaskId, bytes_text, elapsed_text, partial_path};
use super::{Backend, Event};
use crate::dump::{self, DumpFormat, DumpOptions, DumpTarget, Endpoint, RestoreOptions, Tool};
use crate::model::DataSource;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Clone, Debug)]
pub enum ToolTask {
    Dump { options: DumpOptions, target: DumpTarget },
    Restore { options: RestoreOptions, input: PathBuf },
    Psql { options: RestoreOptions, input: PathBuf },
}

#[derive(Clone, Debug)]
pub struct ToolJob {
    pub program: PathBuf,
    pub ds: DataSource,
    pub task: ToolTask,
}

impl ToolJob {
    pub fn tool(&self) -> Tool {
        match self.task {
            ToolTask::Dump { .. } => Tool::PgDump,
            ToolTask::Restore { .. } => Tool::PgRestore,
            ToolTask::Psql { .. } => Tool::Psql,
        }
    }

    /// Arguments for an endpoint; a dump writes to `output` instead of its target.
    pub fn args(&self, endpoint: &Endpoint, output: Option<&Path>) -> Result<Vec<String>, String> {
        match &self.task {
            ToolTask::Dump { options, target } => {
                let mut target = target.clone();
                if let Some(output) = output {
                    target.output = output.to_path_buf();
                }
                dump::dump_args(options, &target, endpoint)
            }
            ToolTask::Restore { options, input } => dump::restore_args(options, input, endpoint),
            ToolTask::Psql { options, input } => dump::psql_args(options, input, endpoint),
        }
    }
}

/// Bytes in a file or, for the directory format, in all its files.
fn size_of(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::read_dir(path)
            .map(|entries| entries.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0),
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

/// Lines sent at once when a tool outputs faster than the progress ticks.
const MAX_PENDING_LINES: usize = 1000;

/// Queues one output line for the next batch sent to the Run log.
fn collect(pending: &mut Vec<LogLine>, last_error: &mut Option<String>, text: String, sender: &super::transfer::TaskSender) {
    let error = is_error_line(&text);
    if error {
        *last_error = Some(text.clone());
    }
    pending.push(LogLine { error, text });
    if pending.len() >= MAX_PENDING_LINES {
        sender.lines(std::mem::take(pending));
    }
}

fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error:") || lower.contains("fatal:") || lower.starts_with("error")
}

impl Backend {
    pub fn run_tool(&self, id: TaskId, job: ToolJob) {
        self.spawn_task(id, move |sender, token| async move {
            let started = std::time::Instant::now();
            let tool = job.tool();
            let mut env = dump::environment(&job.ds).map_err(anyhow::Error::msg)?;
            let mut endpoint = Endpoint::of(&job.ds);
            // The tunnel stays open while the tool runs. The host name stays the
            // server's, so TLS still verifies it; libpq connects to hostaddr.
            let _tunnel = if job.ds.uses_ssh() {
                let tunnel = crate::drivers::open_tunnel(&job.ds).await?;
                endpoint.port = tunnel.local_port;
                env.push(("PGHOSTADDR".into(), "127.0.0.1".into()));
                Some(tunnel)
            } else {
                None
            };
            // Dumps write next to the target and are renamed when complete.
            let (target, written) = match &job.task {
                ToolTask::Dump { options, target } if options.format == DumpFormat::Directory => {
                    anyhow::ensure!(
                        !target.output.exists() || std::fs::read_dir(&target.output)?.next().is_none(),
                        "The directory format needs a new or empty folder: {}",
                        target.output.display()
                    );
                    (Some(target.output.clone()), Some(partial_path(&target.output)))
                }
                ToolTask::Dump { target, .. } => (Some(target.output.clone()), Some(partial_path(&target.output))),
                _ => (None, None),
            };
            if let Some(written) = &written {
                let _ = std::fs::remove_dir_all(written);
                let _ = std::fs::remove_file(written);
            }
            let args = job.args(&endpoint, written.as_deref()).map_err(anyhow::Error::msg)?;
            sender.log(dump::command_line(&job.program, &job.args(&endpoint, None).map_err(anyhow::Error::msg)?));
            let mut child = tokio::process::Command::new(&job.program)
                .args(&args)
                .envs(env)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| anyhow::anyhow!("Cannot start {}: {e}", job.program.display()))?;
            let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
            let mut stderr = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
            let (mut out_open, mut err_open) = (true, true);
            let mut last_error = None;
            let mut lines = 0u64;
            let mut pending: Vec<LogLine> = Vec::new();
            let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
            let cleanup = |written: &Option<PathBuf>| {
                if let Some(written) = written {
                    let _ = std::fs::remove_dir_all(written);
                    let _ = std::fs::remove_file(written);
                }
            };
            let status = loop {
                tokio::select! {
                    line = stdout.next_line(), if out_open => match line? {
                        Some(line) => { lines += 1; collect(&mut pending, &mut last_error, line, &sender) }
                        None => out_open = false,
                    },
                    line = stderr.next_line(), if err_open => match line? {
                        Some(line) => { lines += 1; collect(&mut pending, &mut last_error, line, &sender) }
                        None => err_open = false,
                    },
                    status = child.wait(), if !out_open && !err_open => break status?,
                    () = token.cancelled() => {
                        let _ = child.kill().await;
                        cleanup(&written);
                        return Err(crate::drivers::stopped());
                    }
                    _ = ticker.tick() => {
                        sender.lines(std::mem::take(&mut pending));
                        sender.progress(Progress {
                            bytes: written.as_deref().map_or(0, size_of),
                            rows: lines,
                            ..Progress::default()
                        });
                    }
                }
            };
            sender.lines(pending);
            if !status.success() {
                cleanup(&written);
                let code = status.code().map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
                let reason = last_error.map(|line| format!(": {line}")).unwrap_or_default();
                anyhow::bail!("{} exited with {code}{reason}", tool.binary());
            }
            let elapsed = elapsed_text(started.elapsed());
            match (written, target) {
                (Some(written), Some(target)) => {
                    if target.is_dir() {
                        // An empty folder chosen for the directory format.
                        std::fs::remove_dir(&target)?;
                    }
                    std::fs::rename(&written, &target)?;
                    let size = size_of(&target);
                    sender.progress(Progress { bytes: size, rows: lines, ..Progress::default() });
                    Ok(format!("{} written to {} in {elapsed}", bytes_text(size), target.display()))
                }
                _ => Ok(format!("{} finished in {elapsed}", tool.binary())),
            }
        });
    }

    /// `--version` of a client tool, for the dialog.
    pub fn tool_version(&self, request: u64, program: PathBuf) {
        let send = self.send();
        self.rt.spawn(async move {
            let result = async {
                let output = tokio::process::Command::new(&program).arg("--version").stdin(Stdio::null()).output().await?;
                anyhow::ensure!(output.status.success(), "{} --version failed", program.display());
                anyhow::Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
            .await;
            send(Event::ToolVersion { request, result: result.map_err(super::fmt_err) });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_lines_are_recognised() {
        assert!(is_error_line("pg_dump: error: connection to server failed"));
        assert!(is_error_line("psql:/tmp/a.sql:3: ERROR:  relation \"x\" does not exist"));
        assert!(!is_error_line("pg_dump: dumping contents of table \"shop.orders\""));
    }
}
