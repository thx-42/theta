//! Background jobs of a session (shells, linters, subagents): run beside the main thread, inspectable and killable.
//! When one ends, its result is queued as a message that the agent reads at its next step.

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Output kept per job; older text is dropped.
const OUT_CAP: usize = 64 * 1024;
/// Lines of a finished job's output put in its notification.
const NOTE_LINES: usize = 20;

pub type Work = Pin<Box<dyn Future<Output = i32> + Send>>;
type OnChange = Arc<dyn Fn(Vec<JobInfo>) + Send + Sync>;

/// Output buffer a job writes to while it runs.
#[derive(Clone, Default)]
pub struct Out(Arc<Mutex<String>>);

impl Out {
    pub fn push(&self, s: &str) {
        let mut o = self.0.lock().unwrap();
        o.push_str(s);
        if o.len() > OUT_CAP {
            let cut = o.ceil_char_boundary(o.len() - OUT_CAP);
            o.drain(..cut);
        }
    }
    pub fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Shell,
    Lint,
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Running,
    /// Exit code (0 = success).
    Done(i32),
    Killed,
    /// Linter binary not installed (never ran).
    Missing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    pub kind: Kind,
    pub label: String,
    pub status: Status,
    /// Unix seconds.
    pub started: u64,
    pub ended: Option<u64>,
}

struct Job {
    info: JobInfo,
    out: Out,
    abort: Option<tokio::task::AbortHandle>,
}

#[derive(Default)]
struct Inner {
    jobs: Mutex<Vec<Job>>,
    /// Finished-job messages not yet read by the agent.
    pending: Mutex<Vec<String>>,
    on_change: Mutex<Option<OnChange>>,
    /// File the agent edited last, to lint it once it moves to another one.
    last_edit: Mutex<Option<PathBuf>>,
    /// Linter binaries already reported absent.
    missing: Mutex<Vec<String>>,
}

#[derive(Clone, Default)]
pub struct Jobs(Arc<Inner>);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Jobs {
    /// Call `f` with the job list each time a job starts or ends.
    pub fn on_change(&self, f: impl Fn(Vec<JobInfo>) + Send + Sync + 'static) {
        *self.0.on_change.lock().unwrap() = Some(Arc::new(f));
    }

    pub fn list(&self) -> Vec<JobInfo> {
        self.0.jobs.lock().unwrap().iter().map(|j| j.info.clone()).collect()
    }

    fn changed(&self) {
        let f = self.0.on_change.lock().unwrap().clone();
        if let Some(f) = f {
            f(self.list());
        }
    }

    /// Start `work` in the background and return its id. `quiet_ok`: say nothing when it ends with code 0 and no output.
    pub fn spawn(&self, kind: Kind, label: String, quiet_ok: bool, work: impl FnOnce(Out) -> Work) -> String {
        let id = format!("{:04x}", rand::random::<u16>());
        let out = Out::default();
        let fut = work(out.clone());
        let jobs = self.clone();
        let jid = id.clone();
        // Held until the job is listed, so a job that ends at once still finds itself.
        let mut list = self.0.jobs.lock().unwrap();
        let handle = tokio::spawn(async move {
            let code = fut.await;
            jobs.finish(&jid, code, quiet_ok);
        });
        let info = JobInfo { id: id.clone(), kind, label, status: Status::Running, started: now(), ended: None };
        list.push(Job { info, out, abort: Some(handle.abort_handle()) });
        drop(list);
        self.changed();
        id
    }

    fn finish(&self, id: &str, code: i32, quiet_ok: bool) {
        let note = {
            let mut jobs = self.0.jobs.lock().unwrap();
            let Some(j) = jobs.iter_mut().find(|j| j.info.id == id) else { return };
            if j.info.status != Status::Running {
                return;
            }
            j.info.status = Status::Done(code);
            j.info.ended = Some(now());
            j.abort = None;
            let body = crate::tools::shell::compact(&j.out.text());
            (!(quiet_ok && code == 0 && body.trim().is_empty())).then(|| note(&j.info, &body))
        };
        if let Some(n) = note {
            self.0.pending.lock().unwrap().push(n);
        }
        self.changed();
    }

    /// Stop a running job. False when it does not exist or already ended.
    pub fn kill(&self, id: &str) -> bool {
        let found = {
            let mut jobs = self.0.jobs.lock().unwrap();
            jobs.iter_mut().find(|j| j.info.id == id && j.info.status == Status::Running).map(|j| {
                if let Some(a) = j.abort.take() {
                    a.abort();
                }
                j.info.status = Status::Killed;
                j.info.ended = Some(now());
            })
        };
        if found.is_some() {
            self.changed();
        }
        found.is_some()
    }

    /// Output of job `id` (last `tail` lines when given) and whether it still runs.
    pub fn output(&self, id: &str, tail: Option<usize>) -> Option<(String, JobInfo)> {
        let jobs = self.0.jobs.lock().unwrap();
        let j = jobs.iter().find(|j| j.info.id == id)?;
        let text = crate::tools::shell::compact(&j.out.text());
        let text = match tail {
            Some(n) => {
                let lines: Vec<&str> = text.lines().collect();
                lines[lines.len().saturating_sub(n)..].join("\n")
            }
            None => text,
        };
        Some((text, j.info.clone()))
    }

    /// List an absent linter binary once, so `/jobs` shows it is not installed.
    pub fn note_missing(&self, bin: &str, ext: &str) {
        {
            let mut seen = self.0.missing.lock().unwrap();
            if seen.iter().any(|b| b == bin) {
                return;
            }
            seen.push(bin.to_string());
        }
        let t = now();
        let info = JobInfo { id: format!("{:04x}", rand::random::<u16>()), kind: Kind::Lint, label: format!("{bin} absent (.{ext}): install it to lint"), status: Status::Missing, started: t, ended: Some(t) };
        self.0.jobs.lock().unwrap().push(Job { info, out: Out::default(), abort: None });
        self.changed();
    }

    /// Messages of jobs that ended since the last call.
    pub fn take_pending(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.pending.lock().unwrap())
    }

    /// Record `path` as the file being edited; returns the one edited before it.
    pub fn swap_last_edit(&self, path: Option<PathBuf>) -> Option<PathBuf> {
        std::mem::replace(&mut *self.0.last_edit.lock().unwrap(), path)
    }
}

fn note(i: &JobInfo, body: &str) -> String {
    let state = match i.status {
        Status::Done(0) => "finished".to_string(),
        Status::Done(c) => format!("failed (exit {c})"),
        _ => "stopped".into(),
    };
    let (body, _) = crate::tools::truncate_middle(body, NOTE_LINES);
    let what = match i.kind {
        Kind::Shell => "shell",
        Kind::Lint => "lint",
        Kind::Agent => "subagent",
    };
    format!("[background {what} {} {state}: {}]\n{body}", i.id, i.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_keeps_the_tail() {
        let o = Out::default();
        o.push(&"a".repeat(OUT_CAP));
        o.push("tail");
        assert!(o.text().len() <= OUT_CAP && o.text().ends_with("tail"));
    }

    #[tokio::test]
    async fn job_result_is_queued_and_quiet_ok_is_silent() {
        let jobs = Jobs::default();
        let loud = jobs.spawn(Kind::Shell, "echo".into(), false, |o| Box::pin(async move { o.push("hi\n"); 3 }));
        jobs.spawn(Kind::Lint, "clean".into(), true, |_| Box::pin(async { 0 }));
        for _ in 0..50 {
            if jobs.list().iter().all(|j| j.status != Status::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let notes = jobs.take_pending();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains(&loud) && notes[0].contains("failed (exit 3)") && notes[0].ends_with("hi"));
        assert!(jobs.take_pending().is_empty());
    }

    #[tokio::test]
    async fn kill_stops_a_running_job() {
        let jobs = Jobs::default();
        let id = jobs.spawn(Kind::Shell, "sleep".into(), false, |_| Box::pin(async { tokio::time::sleep(std::time::Duration::from_secs(60)).await; 0 }));
        assert!(jobs.kill(&id) && !jobs.kill(&id));
        assert_eq!(jobs.list()[0].status, Status::Killed);
        assert!(jobs.take_pending().is_empty());
    }
}
