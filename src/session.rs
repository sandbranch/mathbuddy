//! Finding the sessions mathbuddy could attach to, suggesting one, and the
//! connection to the one it is attached to.
//!
//! Two kinds: Jupyter kernels (jotter, euporie, Jupyter servers, ...), found
//! by their connection files, and R sessions running mathbuddy's bridge
//! (RStudio, terminal R), found by the bridge's registry files.
//!
//! The suggestion prefers, in order: the session mathbuddy was started from
//! inside (RStudio's Terminal tab is a child of its R session), a session in
//! the same tmux window, a session in the same folder, and then the newest.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{kernel::KernelWatch, rbridge::RBridge};

/// One thing the user ran, as mathbuddy saw it.
#[derive(Clone, Debug)]
pub struct Cell {
    pub msg_id: String,
    pub count: Option<i64>,
    pub code: String,
    pub outputs: Vec<String>,
    pub error: Option<String>,
    pub done: bool,
}

impl Cell {
    pub fn new(msg_id: String, count: Option<i64>, code: String) -> Self {
        Cell {
            msg_id,
            count,
            code,
            outputs: Vec::new(),
            error: None,
            done: false,
        }
    }

    pub fn render(&self, code_max: usize, out_max: usize) -> String {
        let n = self
            .count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        let code = if self.code.chars().count() <= code_max {
            self.code.clone()
        } else {
            format!(
                "{}\n# ...(truncated)",
                self.code.chars().take(code_max).collect::<String>()
            )
        };
        let mut parts = vec![format!("In [{n}]:\n{code}")];
        let out = self.outputs.concat();
        let out = out.trim();
        if !out.is_empty() {
            let chars: Vec<char> = out.chars().collect();
            let out = if chars.len() > out_max {
                let head: String = chars[..out_max / 2].iter().collect();
                let tail: String = chars[chars.len() - out_max / 2..].iter().collect();
                format!("{head}\n...(truncated)...\n{tail}")
            } else {
                out.to_string()
            };
            parts.push(format!("Out [{n}]:\n{out}"));
        }
        if let Some(e) = &self.error {
            parts.push(format!("Error [{n}]:\n{e}"));
        }
        if !self.done {
            parts.push("(still running)".into());
        }
        parts.join("\n")
    }
}

/// A cell that finished with an error (for the automatic hint).
#[derive(Clone, Debug)]
pub struct CellDone {
    pub cell: Cell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Jupyter,
    RBridge,
}

/// A session mathbuddy could attach to.
#[derive(Clone, Debug)]
pub struct Found {
    pub kind: Kind,
    pub path: PathBuf,
    pub language: String,
    pub frontend: String,
    pub label: String,
    pub started: f64,
    /// The frontend's process (jotter, euporie) or R itself.
    pub pid: Option<u32>,
    pub cwd: String,
    pub score: u8,
    pub reason: String,
}

impl Found {
    pub fn title(&self) -> String {
        format!("{} · {} · {}", self.language, self.frontend, self.label)
    }

    pub fn line(&self) -> String {
        let age = now() - self.started;
        let when = if age < 3600.0 {
            format!("{} min ago", (age / 60.0) as i64)
        } else {
            format!("{:.1} h ago", age / 3600.0)
        };
        let why = if self.reason.is_empty() {
            String::new()
        } else {
            format!("  ← {}", self.reason)
        };
        format!("{}  (started {when}){why}", self.title())
    }

    pub fn matches(&self, words: &str) -> bool {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let hay = format!(
            "{} {} {} {name} {}",
            self.language, self.frontend, self.label, self.cwd
        )
        .to_lowercase();
        words
            .to_lowercase()
            .split_whitespace()
            .all(|w| hay.contains(w))
    }

    pub async fn connect(&self, done_tx: mpsc::UnboundedSender<CellDone>) -> Result<Connection> {
        Ok(match self.kind {
            Kind::RBridge => Connection::R(RBridge::connect(&self.path, done_tx).await?),
            Kind::Jupyter => Connection::Kernel(KernelWatch::connect(&self.path, done_tx).await?),
        })
    }
}

/// The session mathbuddy is attached to: the same surface for both kinds,
/// so the assistant can't tell them apart.
pub enum Connection {
    Kernel(KernelWatch),
    R(RBridge),
}

impl Connection {
    pub fn path(&self) -> &Path {
        match self {
            Connection::Kernel(k) => &k.cf,
            Connection::R(r) => &r.cf,
        }
    }

    pub fn language(&self) -> String {
        match self {
            Connection::Kernel(k) => k.language.clone(),
            Connection::R(_) => "R".into(),
        }
    }

    pub fn kernel_name(&self) -> String {
        match self {
            Connection::Kernel(k) => k.kernel_name.clone(),
            Connection::R(r) => r.kernel_name(),
        }
    }

    /// The program the user works in, for the system prompt.
    pub fn frontend(&self) -> String {
        match self {
            Connection::Kernel(_) => "a Jupyter frontend (jotter)".into(),
            Connection::R(r) => r.frontend.clone(),
        }
    }

    pub fn can_inspect(&self) -> bool {
        match self {
            Connection::Kernel(k) => k.can_inspect(),
            Connection::R(_) => true,
        }
    }

    pub fn info(&self) -> Value {
        match self {
            Connection::Kernel(_) => Value::Null,
            Connection::R(r) => r.info(),
        }
    }

    /// RStudio with rstudioapi: /insert and /console work.
    pub fn rstudio_api(&self) -> bool {
        self.info()["rstudio_api"].as_bool().unwrap_or(false)
    }

    pub async fn list_variables(&self) -> Result<String> {
        match self {
            Connection::Kernel(k) => k.list_variables().await,
            Connection::R(r) => r.list_variables().await,
        }
    }

    pub async fn inspect(&self, expr: &str) -> Result<String> {
        match self {
            Connection::Kernel(k) => k.inspect(expr).await,
            Connection::R(r) => r.inspect(expr).await,
        }
    }

    pub async fn run(&self, code: &str) -> Result<(String, String)> {
        match self {
            Connection::Kernel(k) => k.run(code).await,
            Connection::R(r) => r.run(code).await,
        }
    }

    /// RStudio only: put code in the open script, or send it to the console.
    pub async fn to_editor(&self, console: bool, code: &str) -> Result<String> {
        match self {
            Connection::R(r) => {
                r.editor(if console { "/console" } else { "/insert" }, code)
                    .await
            }
            Connection::Kernel(_) => anyhow::bail!("works in RStudio sessions; use /copy here"),
        }
    }

    pub fn recent(&self, n: usize) -> Vec<Cell> {
        match self {
            Connection::Kernel(k) => k.recent(n),
            Connection::R(r) => r.recent(n),
        }
    }

    pub async fn alive(&self) -> bool {
        match self {
            Connection::Kernel(k) => k.alive().await,
            Connection::R(r) => r.alive(),
        }
    }
}

// -- /proc helpers ---------------------------------------------------------

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|&c| c == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn ppid(pid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    s.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn cwd(pid: u32) -> String {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn ancestors(mut pid: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    while pid > 1 && chain.len() < 64 {
        chain.push(pid);
        match ppid(pid) {
            Some(p) => pid = p,
            None => break,
        }
    }
    chain
}

/// Connection file -> kernel pid, from the kernels' command lines.
fn kernel_pids() -> HashMap<PathBuf, u32> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        for a in cmdline(pid) {
            if a.ends_with(".json") && (a.contains("jotter-") || a.contains("kernel-")) {
                let p = std::fs::canonicalize(&a).unwrap_or_else(|_| PathBuf::from(&a));
                out.insert(p, pid);
            }
        }
    }
    out
}

fn language_of(cmd: &[String], cf: &Path) -> String {
    let s = cmd.join(" ").to_lowercase();
    for (key, name) in [
        ("ipykernel", "Python"),
        ("irkernel", "R"),
        ("octave", "Octave"),
        ("wolfram", "Wolfram"),
        ("julia", "Julia"),
        ("giac", "Giac"),
    ] {
        if s.contains(key) {
            return name.into();
        }
    }
    std::fs::read_to_string(cf)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v["kernel_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| "unknown".into())
}

/// (frontend name, label, frontend pid) for the process that started a kernel.
fn frontend_of(pid: Option<u32>) -> (String, String, Option<u32>) {
    let Some(pid) = pid else {
        return ("Jupyter".into(), "?".into(), None);
    };
    let cmd = cmdline(pid);
    let base = |s: &str| {
        Path::new(s)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let mut exe = cmd.first().map(|c| base(c)).unwrap_or_else(|| "?".into());
    if exe.starts_with("python") && cmd.len() > 1 {
        exe = base(&cmd[1]);
    }
    let nb = cmd.iter().skip(1).find(|a| a.ends_with(".ipynb"));
    let name = match exe.as_str() {
        "jotter" => "jotter".to_string(),
        "euporie" | "euporie-notebook" => "euporie".to_string(),
        _ if cmd.join(" ").contains("jupyter") => "Jupyter".to_string(),
        _ => exe.clone(),
    };
    let label = match nb {
        Some(nb) => base(nb),
        None if name == "jotter" => "scratchpad".into(),
        None => {
            let c = cwd(pid);
            let b = base(&c);
            if b.is_empty() { "?".into() } else { b }
        }
    };
    (name, label, Some(pid))
}

pub const JUPYTER_PATTERNS: [&str; 2] = ["jotter-*.json", "kernel-*.json"];

/// Connection files found dead, with their mtime: they don't come back.
static DEAD: Mutex<Option<HashMap<PathBuf, SystemTime>>> = Mutex::new(None);

pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks the process exists
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub async fn discover(patterns: Option<&[String]>, probe_r: bool) -> Vec<Found> {
    let mut found = Vec::new();
    let pats: Vec<String> = match patterns {
        Some(p) => p.to_vec(),
        None => JUPYTER_PATTERNS.iter().map(|s| s.to_string()).collect(),
    };
    let mut pids: Option<HashMap<PathBuf, u32>> = None;
    for pat in &pats {
        for cf in crate::kernel::candidate_files(pat) {
            let real = std::fs::canonicalize(&cf).unwrap_or_else(|_| cf.clone());
            let Ok(mtime) = std::fs::metadata(&cf).and_then(|m| m.modified()) else {
                continue;
            };
            if DEAD
                .lock()
                .unwrap()
                .get_or_insert_with(HashMap::new)
                .get(&real)
                == Some(&mtime)
            {
                continue;
            }
            if !crate::kernel::kernel_alive(&cf, Duration::from_millis(300)).await {
                DEAD.lock()
                    .unwrap()
                    .get_or_insert_with(HashMap::new)
                    .insert(real, mtime);
                continue;
            }
            let pids = pids.get_or_insert_with(kernel_pids);
            let kpid = pids.get(&real).copied();
            let (front, label, fpid) = frontend_of(kpid.and_then(ppid));
            let cmd = kpid.map(cmdline).unwrap_or_default();
            let started = mtime
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            found.push(Found {
                kind: Kind::Jupyter,
                language: language_of(&cmd, &cf),
                path: cf,
                frontend: front,
                label,
                started,
                pid: fpid,
                cwd: fpid.map(cwd).unwrap_or_default(),
                score: 0,
                reason: String::new(),
            });
        }
    }
    let Ok(rd) = std::fs::read_dir(crate::rbridge::bridge_dir()) else {
        return found;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("r-") && name.ends_with(".json")) {
            continue;
        }
        let reg = e.path();
        let Some(info) = std::fs::read_to_string(&reg)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        let Some(pid) = info["pid"].as_u64().map(|p| p as u32) else {
            continue;
        };
        if !pid_alive(pid) {
            let _ = std::fs::remove_file(&reg); // our own registry file, left by an R that was killed
            continue;
        }
        let r_cwd = info["cwd"].as_str().unwrap_or("").to_string();
        let mut label = Path::new(&r_cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if label.is_empty() {
            label = "R".into();
        }
        if probe_r {
            // the project name, if R is idle enough to say
            if let Some(l) = crate::rbridge::probe_label(&info).await {
                label = l;
            }
        }
        let started = info["started"].as_f64().unwrap_or_else(|| {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        });
        found.push(Found {
            kind: Kind::RBridge,
            path: reg,
            language: "R".into(),
            frontend: info["frontend"].as_str().unwrap_or("R").into(),
            label,
            started,
            pid: Some(pid),
            cwd: r_cwd,
            score: 0,
            reason: String::new(),
        });
    }
    found
}

fn tmux_window_pane_pids() -> Vec<u32> {
    let (Ok(_), Ok(pane)) = (std::env::var("TMUX"), std::env::var("TMUX_PANE")) else {
        return Vec::new();
    };
    let run = |args: &[&str]| {
        std::process::Command::new("tmux")
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let Some(win) = run(&["display-message", "-p", "-t", &pane, "#{window_id}"]) else {
        return Vec::new();
    };
    run(&["list-panes", "-t", win.trim(), "-F", "#{pane_pid}"])
        .map(|s| {
            s.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Score and order sessions, best suggestion first.
pub fn rank(mut sessions: Vec<Found>) -> Vec<Found> {
    let mine = ancestors(std::process::id());
    let panes = tmux_window_pane_pids();
    let here = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    for s in &mut sessions {
        (s.score, s.reason) = (0, String::new());
        let Some(pid) = s.pid else { continue };
        let chain = ancestors(pid);
        if mine.contains(&pid) {
            (s.score, s.reason) = (
                3,
                format!("you started mathbuddy from inside this {}", s.frontend),
            );
        } else if !panes.is_empty() && chain.iter().any(|p| panes.contains(p)) {
            (s.score, s.reason) = (2, "next to you in this tmux window".into());
        } else if !s.cwd.is_empty() && s.cwd == here {
            (s.score, s.reason) = (1, "same folder as you".into());
        }
    }
    sessions.sort_by(|a, b| {
        (b.score, b.started)
            .partial_cmp(&(a.score, a.started))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(first) = sessions.first_mut()
        && first.reason.is_empty()
    {
        first.reason = "the newest".into();
    }
    sessions
}

/// `target`: None (best suggestion), a 1-based number, or words to match.
pub fn pick<'a>(sessions: &'a [Found], target: Option<&str>) -> Option<&'a Found> {
    match target {
        None | Some("") => sessions.first(),
        Some(t) if t.chars().all(|c| c.is_ascii_digit()) => t
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| sessions.get(i)),
        Some(t) => sessions.iter().find(|s| s.matches(t)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(label: &str, started: f64) -> Found {
        Found {
            kind: Kind::Jupyter,
            path: PathBuf::from(format!("/run/jotter-{label}.json")),
            language: "Python".into(),
            frontend: "jotter".into(),
            label: label.into(),
            started,
            pid: None,
            cwd: String::new(),
            score: 0,
            reason: String::new(),
        }
    }

    #[test]
    fn newest_first_without_hints() {
        let r = rank(vec![found("old.ipynb", 1.0), found("new.ipynb", 2.0)]);
        assert_eq!(r[0].label, "new.ipynb");
        assert_eq!(r[0].reason, "the newest");
    }

    #[test]
    fn pick_by_number_or_words() {
        let s = vec![found("demo.ipynb", 2.0), found("assay.ipynb", 1.0)];
        assert_eq!(pick(&s, None).unwrap().label, "demo.ipynb");
        assert_eq!(pick(&s, Some("2")).unwrap().label, "assay.ipynb");
        assert_eq!(pick(&s, Some("jotter assay")).unwrap().label, "assay.ipynb");
        assert!(pick(&s, Some("3")).is_none() && pick(&s, Some("rstudio")).is_none());
    }

    #[test]
    fn cell_render_truncates_and_marks_running() {
        let mut c = Cell::new("x".into(), Some(3), "print(1)".into());
        c.outputs.push("a".repeat(2000));
        let r = c.render(1500, 800);
        assert!(
            r.starts_with("In [3]:\nprint(1)")
                && r.contains("...(truncated)...")
                && r.ends_with("(still running)")
        );
    }

    #[test]
    fn ancestors_of_self_include_parent() {
        let me = std::process::id();
        let chain = ancestors(me);
        assert_eq!(chain.first(), Some(&me));
        assert!(chain.len() >= 2);
    }
}
