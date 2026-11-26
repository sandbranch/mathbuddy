//! Attach to a running Jupyter kernel (jotter's, or any frontend's), watch
//! it, inspect it, and run code in it when the user says so.
//!
//! Everything here is the plain Jupyter protocol, so it works with any
//! frontend that leaves a connection file behind. The kernel broadcasts every
//! cell's code, output and error on IOPub to all connected clients; we are
//! one more client listening, with our own shell socket for our requests.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use jupyter_protocol::{
    ConnectionInfo,
    messaging::{
        ExecuteRequest, HistoryRequest, JupyterMessage, JupyterMessageContent, KernelInfoRequest,
    },
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::session::{Cell, CellDone};

/// Cells kept per session.
pub const MAX_CELLS: usize = 60;

/// Jupyter's runtime directories, where frontends leave connection files.
pub fn runtime_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(x) = std::env::var("JUPYTER_RUNTIME_DIR") {
        dirs.push(PathBuf::from(x));
    }
    if let Ok(x) = std::env::var("XDG_RUNTIME_DIR") {
        dirs.push(Path::new(&x).join("jupyter"));
    }
    dirs.push(jupyter_zmq_client::runtime_dir());
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if d.is_dir() && !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// Connection files matching a glob-like `prefix*suffix` pattern, newest first.
pub fn candidate_files(pattern: &str) -> Vec<PathBuf> {
    let (pre, suf) = pattern.split_once('*').unwrap_or((pattern, ""));
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for d in runtime_dirs() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(pre) && name.ends_with(suf) && name.len() >= pre.len() + suf.len() {
                let t = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                let p = e.path();
                if !files.iter().any(|(_, q)| q == &p) {
                    files.push((t, p));
                }
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().map(|(_, p)| p).collect()
}

fn read_info(cf: &Path) -> Result<ConnectionInfo> {
    let text = std::fs::read_to_string(cf).with_context(|| format!("reading {}", cf.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", cf.display()))
}

/// Ping the kernel's heartbeat socket. The heartbeat is answered by its own
/// thread in the kernel, so a kernel busy with a long cell still counts as
/// alive, and a stale connection file (jotter leaves them behind) does not.
pub async fn kernel_alive(cf: &Path, timeout: Duration) -> bool {
    let Ok(info) = read_info(cf) else {
        return false;
    };
    let ping = async {
        let mut hb = jupyter_zmq_client::create_client_heartbeat_connection(&info)
            .await
            .ok()?;
        hb.single_heartbeat().await.ok()
    };
    matches!(tokio::time::timeout(timeout, ping).await, Ok(Some(())))
}

/// What the model may ask to inspect: a name, attribute access, simple
/// subscripts. No calls, so an "inspection" can't change the user's state.
fn inspectable(expr: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^[A-Za-z_]\w*(\.[A-Za-z_]\w*|\[[^\[\]()]{1,40}\])*$").unwrap()
    })
    .is_match(expr)
}

/// Python helpers defined (silently) in the user's kernel, so listing and
/// describing variables is one user expression each.
const PY_HELPER: &str = r#"
def _mathbuddy_vars():
    import types
    skip = {"In", "Out", "exit", "quit", "get_ipython", "open"}
    rows = []
    for k, v in list(globals().items()):
        if k.startswith("_") or k in skip or isinstance(v, types.ModuleType):
            continue
        t = type(v).__name__
        extra = ""
        shape = getattr(v, "shape", None)
        if shape is not None and not callable(shape):
            try:
                extra = f" shape={tuple(shape)}"
            except Exception:
                pass
        elif isinstance(v, (list, tuple, dict, set, str)):
            extra = f" len={len(v)}"
        dt = getattr(v, "dtype", None)
        if dt is not None and not callable(dt):
            extra += f" dtype={dt}"
        rows.append(f"{k}: {t}{extra}")
    return "\n".join(rows[:300]) or "(no user variables)"

def _mathbuddy_describe(expr, maxlen=2500):
    v = eval(expr, globals())
    t = type(v)
    lines = [f"{expr}: {t.__module__}.{t.__qualname__}"]
    try:
        import pandas as pd
        if isinstance(v, pd.DataFrame):
            lines.append(f"shape={v.shape}")
            lines.append("dtypes:\n" + v.dtypes.to_string())
            lines.append("head:\n" + v.head(8).to_string())
            return "\n".join(lines)[:maxlen]
        if isinstance(v, pd.Series):
            lines.append(f"len={len(v)} dtype={v.dtype} name={v.name!r}")
            lines.append("head:\n" + v.head(8).to_string())
            return "\n".join(lines)[:maxlen]
    except ImportError:
        pass
    shape = getattr(v, "shape", None)
    if shape is not None and not callable(shape):
        lines.append(f"shape={tuple(shape)} dtype={getattr(v, 'dtype', '?')}")
        try:
            import numpy as np
            a = np.asarray(v)
            if a.size and a.size <= 50_000_000 and np.issubdtype(a.dtype, np.number):
                lines.append(f"min={a.min()!r} max={a.max()!r} mean={a.mean()!r}")
        except Exception:
            pass
    if callable(v):
        import inspect
        try:
            lines.append(f"signature: {expr}{inspect.signature(v)}")
        except Exception:
            pass
        doc = (getattr(v, "__doc__", None) or "").strip()
        if doc:
            lines.append("doc: " + doc[:600])
    r = repr(v)
    if len(r) > maxlen:
        r = r[: maxlen // 2] + " ... " + r[-maxlen // 4 :]
    lines.append("repr: " + r)
    return "\n".join(lines)[:maxlen]
"#;

/// Messages on IOPub that answer one of our own requests.
type Collector = mpsc::UnboundedSender<Option<Value>>;

struct Shared {
    cells: VecDeque<Cell>,
    /// Our own requests' msg_ids: their IOPub traffic isn't the user's.
    mine: HashMap<String, Collector>,
    helper_ready: bool,
}

/// One connection to one kernel: watches IOPub, runs our own requests.
pub struct KernelWatch {
    pub cf: PathBuf,
    pub language: String,
    pub kernel_name: String,
    session: String,
    shell: tokio::sync::Mutex<jupyter_zmq_client::ClientShellConnection>,
    shared: Arc<Mutex<Shared>>,
    iopub_task: tokio::task::AbortHandle,
}

impl Drop for KernelWatch {
    fn drop(&mut self) {
        self.iopub_task.abort();
    }
}

impl KernelWatch {
    pub async fn connect(cf: &Path, done_tx: mpsc::UnboundedSender<CellDone>) -> Result<Self> {
        let info = read_info(cf)?;
        let session = uuid::Uuid::new_v4().to_string();
        let iopub = jupyter_zmq_client::create_client_iopub_connection(&info, "", &session)
            .await
            .map_err(|e| anyhow!("iopub connect: {e}"))?;
        let identity =
            jupyter_zmq_client::peer_identity_for_session(&session).map_err(|e| anyhow!("{e}"))?;
        let shell = jupyter_zmq_client::create_client_shell_connection_with_identity(
            &info, &session, identity,
        )
        .await
        .map_err(|e| anyhow!("shell connect: {e}"))?;
        let shared = Arc::new(Mutex::new(Shared {
            cells: VecDeque::new(),
            mine: HashMap::new(),
            helper_ready: false,
        }));
        let iopub_task = tokio::spawn(iopub_loop(iopub, shared.clone(), done_tx)).abort_handle();
        let mut k = KernelWatch {
            cf: cf.to_path_buf(),
            language: "unknown".into(),
            kernel_name: cf
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            session,
            shell: tokio::sync::Mutex::new(shell),
            shared,
            iopub_task,
        };
        k.load_info().await;
        k.seed_history().await;
        Ok(k)
    }

    /// Send a request on our shell socket and wait for its reply. Our msg_id
    /// is registered first, so the IOPub task never mistakes our traffic for
    /// the user's. With `collect`, also gather its IOPub output until idle.
    async fn request(
        &self,
        content: impl Into<JupyterMessageContent>,
        timeout: Duration,
        collect: bool,
    ) -> Result<(Value, Vec<Value>)> {
        let msg = JupyterMessage::new(content.into(), None).with_session(&self.session);
        let id = msg.header.msg_id.clone();
        let kind = msg.header.msg_type.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.shared.lock().unwrap().mine.insert(id.clone(), tx);
        let result = async {
            let mut shell = self.shell.lock().await;
            shell.send(msg).await.map_err(|e| anyhow!("send: {e}"))?;
            let reply = loop {
                let r = shell.read().await.map_err(|e| anyhow!("shell read: {e}"))?;
                if r.parent_header.as_ref().is_some_and(|h| h.msg_id == id) {
                    break serde_json::to_value(&r.content)?;
                }
            };
            drop(shell);
            let mut outputs = Vec::new();
            if collect {
                while let Some(Some(m)) = rx.recv().await {
                    outputs.push(m);
                }
            }
            Ok::<_, anyhow::Error>((reply, outputs))
        };
        let out = tokio::time::timeout(timeout, result).await;
        self.shared.lock().unwrap().mine.remove(&id);
        match out {
            Ok(r) => r,
            Err(_) => bail!(
                "kernel did not answer {kind} in {}s (is it busy?)",
                timeout.as_secs()
            ),
        }
    }

    async fn load_info(&mut self) {
        if let Ok((reply, _)) = self
            .request(KernelInfoRequest {}, Duration::from_secs(10), false)
            .await
        {
            let li = &reply["language_info"];
            self.language = li["name"].as_str().unwrap_or("unknown").to_string();
            let imp = reply["implementation"].as_str().unwrap_or("");
            self.kernel_name = if imp.is_empty() {
                self.language.clone()
            } else {
                format!("{} ({imp})", self.language)
            };
        }
    }

    /// Recent inputs from before we attached, so we're not starting blind.
    /// Only the current session (session 0 in IPython's numbering): older
    /// sessions' code would read as what the user is doing now.
    async fn seed_history(&self) {
        let req = HistoryRequest::Range {
            session: Some(0),
            start: 1,
            stop: 1_000_000,
            output: false,
            raw: true,
        };
        let Ok((reply, _)) = self.request(req, Duration::from_secs(5), false).await else {
            return;
        };
        let Some(hist) = reply["history"].as_array() else {
            return;
        };
        let start = hist.len().saturating_sub(12);
        let mut sh = self.shared.lock().unwrap();
        for h in &hist[start..] {
            let (Some(sess), Some(line), Some(src)) = (h[0].as_i64(), h[1].as_i64(), h[2].as_str())
            else {
                continue;
            };
            if src.trim().is_empty() || src.contains("_mathbuddy") {
                continue;
            }
            let mut c = Cell::new(
                format!("history-{sess}-{line}"),
                Some(line),
                src.to_string(),
            );
            c.done = true;
            push_cell(&mut sh.cells, c);
        }
    }

    pub fn is_python(&self) -> bool {
        self.language == "python"
    }

    pub fn is_giac(&self) -> bool {
        self.language == "giac"
    }

    /// Variables can be looked at without running anything visible: Python
    /// (through a silent helper) and Giac (whose kernel evaluates
    /// user_expressions).
    pub fn can_inspect(&self) -> bool {
        self.is_python() || self.is_giac()
    }

    async fn ensure_helper(&self) -> Result<()> {
        if !self.is_python() || self.shared.lock().unwrap().helper_ready {
            return Ok(());
        }
        let req = ExecuteRequest {
            code: PY_HELPER.into(),
            silent: true,
            store_history: false,
            user_expressions: None,
            allow_stdin: false,
            stop_on_error: false,
        };
        self.request(req, Duration::from_secs(30), false).await?;
        self.shared.lock().unwrap().helper_ready = true;
        Ok(())
    }

    /// Evaluate expressions silently: nothing appears in the frontend, no
    /// history entry, the results come back in the reply only.
    async fn user_exprs(
        &self,
        exprs: &[(&str, String)],
    ) -> Result<HashMap<String, Result<String, String>>> {
        let req = ExecuteRequest {
            code: String::new(),
            silent: true,
            store_history: false,
            user_expressions: Some(
                exprs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ),
            allow_stdin: false,
            stop_on_error: false,
        };
        let (reply, _) = self.request(req, Duration::from_secs(20), false).await?;
        let mut out = HashMap::new();
        for (k, _) in exprs {
            let r = &reply["user_expressions"][*k];
            let v = if r["status"] == "ok" {
                Ok(r["data"]["text/plain"].as_str().unwrap_or("").to_string())
            } else {
                Err(format!(
                    "{}: {}",
                    r["ename"].as_str().unwrap_or("Error"),
                    r["evalue"].as_str().unwrap_or("no result")
                ))
            };
            out.insert(k.to_string(), v);
        }
        Ok(out)
    }

    /// One Python helper call; reinstalls the helper once if the kernel
    /// restarted under us.
    async fn py_call(&self, expr: String) -> Result<String> {
        for attempt in 0..2 {
            self.ensure_helper().await?;
            let mut r = self.user_exprs(&[("r", expr.clone())]).await?;
            match r.remove("r").unwrap_or(Err("no result".into())) {
                Ok(text) => return Ok(python_str_literal(&text).unwrap_or(text)),
                Err(e)
                    if attempt == 0 && e.starts_with("NameError") && e.contains("_mathbuddy") =>
                {
                    self.shared.lock().unwrap().helper_ready = false;
                }
                Err(e) => return Ok(e),
            }
        }
        Ok("error".into())
    }

    pub async fn list_variables(&self) -> Result<String> {
        if self.is_python() {
            return self.py_call("_mathbuddy_vars()".into()).await;
        }
        if self.is_giac() {
            let mut r = self.user_exprs(&[("v", "VARS()".into())]).await?;
            return Ok(match r.remove("v").unwrap_or(Err("no result".into())) {
                Ok(v) if v.trim().is_empty() || v.trim() == "[]" => "(no user variables)".into(),
                Ok(v) => format!("variables: {v}"),
                Err(e) => e,
            });
        }
        Ok(format!(
            "(variable listing is implemented for Python and Giac kernels; this is {})",
            self.language
        ))
    }

    pub async fn inspect(&self, expr: &str) -> Result<String> {
        let expr = expr.trim();
        if !self.can_inspect() {
            return Ok(format!(
                "(inspection is implemented for Python and Giac kernels; this is {})",
                self.language
            ));
        }
        if !inspectable(expr) {
            return Ok(
                "refused: only names, attributes and simple subscripts can be inspected (no calls)"
                    .into(),
            );
        }
        if self.is_python() {
            return self
                .py_call(format!("_mathbuddy_describe({})", python_repr(expr)))
                .await;
        }
        let mut r = self
            .user_exprs(&[("v", expr.to_string()), ("t", format!("type({expr})"))])
            .await?;
        let v = r.remove("v").unwrap_or(Err("no result".into()));
        let t = r.remove("t").and_then(Result::ok).unwrap_or_default();
        Ok(match v {
            Ok(v) => format!("{expr}: {t}\nvalue: {}", truncate(&v, 2500)),
            Err(e) => e,
        })
    }

    /// Run code in the user's namespace on their say-so. Kept in kernel
    /// history (In[]) so it isn't invisible, but it is not a notebook cell.
    pub async fn run(&self, code: &str) -> Result<(String, String)> {
        let req = ExecuteRequest {
            code: code.into(),
            silent: false,
            store_history: true,
            user_expressions: None,
            allow_stdin: false,
            stop_on_error: false,
        };
        let (reply, outputs) = self.request(req, Duration::from_secs(300), true).await?;
        let mut text = String::new();
        let mut plots = 0;
        for m in &outputs {
            match m["msg_type"].as_str().unwrap_or("") {
                "stream" => text.push_str(m["content"]["text"].as_str().unwrap_or("")),
                "execute_result" | "display_data" => {
                    let d = &m["content"]["data"];
                    let keys: Vec<&str> = d
                        .as_object()
                        .map(|o| o.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    if keys.iter().any(|k| k.starts_with("image/")) {
                        plots += 1;
                    } else {
                        match d["text/plain"].as_str() {
                            Some(t) => text.push_str(t),
                            None => text.push_str(&format!("[{}]", keys.join(", "))),
                        }
                        text.push('\n');
                    }
                }
                "error" => {
                    text.push_str(&error_text(&m["content"], usize::MAX));
                    text.push('\n');
                }
                _ => {}
            }
        }
        if plots > 0 {
            text.push_str(&format!(
                "(drew {plots} plot{}; it isn't shown in the notebook because mathbuddy ran the code, \
                 not a cell: /copy it into a cell to see it)\n",
                if plots > 1 { "s" } else { "" }
            ));
        }
        Ok((reply["status"].as_str().unwrap_or("?").to_string(), text))
    }

    pub fn recent(&self, n: usize) -> Vec<Cell> {
        let sh = self.shared.lock().unwrap();
        sh.cells
            .iter()
            .skip(sh.cells.len().saturating_sub(n))
            .cloned()
            .collect()
    }

    pub async fn alive(&self) -> bool {
        kernel_alive(&self.cf, Duration::from_secs(2)).await
    }
}

fn push_cell(cells: &mut VecDeque<Cell>, c: Cell) {
    cells.retain(|x| x.msg_id != c.msg_id);
    cells.push_back(c);
    while cells.len() > MAX_CELLS {
        cells.pop_front();
    }
}

/// Everyone's IOPub traffic: the user's cells (tracked), ours (forwarded to
/// the request waiting for it).
async fn iopub_loop(
    mut iopub: jupyter_zmq_client::ClientIoPubConnection,
    shared: Arc<Mutex<Shared>>,
    done_tx: mpsc::UnboundedSender<CellDone>,
) {
    loop {
        let Ok(msg) = iopub.read().await else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let parent = msg
            .parent_header
            .as_ref()
            .map(|h| h.msg_id.clone())
            .unwrap_or_default();
        let kind = msg.header.msg_type.clone();
        let Ok(content) = serde_json::to_value(&msg.content) else {
            continue;
        };
        let mut sh = shared.lock().unwrap();
        if let Some(tx) = sh.mine.get(&parent) {
            if kind == "status" && content["execution_state"] == "idle" {
                let _ = tx.send(None);
            } else if matches!(
                kind.as_str(),
                "stream" | "execute_result" | "display_data" | "error"
            ) {
                let _ = tx.send(Some(
                    serde_json::json!({"msg_type": kind, "content": content}),
                ));
            }
            continue;
        }
        if kind == "status" && content["execution_state"] == "starting" {
            sh.helper_ready = false;
            continue;
        }
        if kind == "execute_input" {
            let c = Cell::new(
                parent,
                content["execution_count"].as_i64(),
                content["code"].as_str().unwrap_or("").into(),
            );
            push_cell(&mut sh.cells, c);
            continue;
        }
        let Some(cell) = sh.cells.iter_mut().find(|c| c.msg_id == parent) else {
            continue;
        };
        match kind.as_str() {
            "stream" => cell
                .outputs
                .push(content["text"].as_str().unwrap_or("").into()),
            "execute_result" | "display_data" => {
                let d = &content["data"];
                if let Some(t) = d["text/plain"].as_str() {
                    cell.outputs.push(format!("{t}\n"));
                }
                let others: Vec<&str> = d
                    .as_object()
                    .map(|o| {
                        o.keys()
                            .map(String::as_str)
                            .filter(|k| *k != "text/plain")
                            .collect()
                    })
                    .unwrap_or_default();
                if !others.is_empty() {
                    cell.outputs
                        .push(format!("[rich output: {}]\n", others.join(", ")));
                }
            }
            "error" => cell.error = Some(error_text(&content, 25)),
            "status" if content["execution_state"] == "idle" => {
                cell.done = true;
                if cell.error.is_some() {
                    let _ = done_tx.send(CellDone { cell: cell.clone() });
                }
            }
            _ => {}
        }
    }
}

/// An error output as text: the traceback without colour codes, the middle
/// left out when it runs past `max_lines` (its point is its end).
fn error_text(c: &Value, max_lines: usize) -> String {
    let tb: Vec<String> = c["traceback"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(strip_ansi).collect())
        .unwrap_or_default();
    let tb = tb.join("\n");
    if tb.trim().is_empty() {
        return format!(
            "{}: {}",
            c["ename"].as_str().unwrap_or("Error"),
            c["evalue"].as_str().unwrap_or("")
        );
    }
    let lines: Vec<&str> = tb.lines().collect();
    if lines.len() > max_lines {
        let mut out: Vec<&str> = lines[..5].to_vec();
        out.push("...");
        out.extend(&lines[lines.len() - 18..]);
        return out.join("\n");
    }
    tb
}

pub fn strip_ansi(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap())
        .replace_all(s, "")
        .into_owned()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max / 2).collect();
    let tail: String = s.chars().skip(s.chars().count() - max / 4).collect();
    format!("{head} ... {tail}")
}

/// `repr()` of a string, for building a Python call.
fn python_repr(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The value of a Python string literal (what `repr()` of a str gives), or
/// None if `s` isn't one.
pub fn python_str_literal(s: &str) -> Option<String> {
    let s = s.trim();
    let q = s.chars().next()?;
    if !(q == '\'' || q == '"') || s.len() < 2 || !s.ends_with(q) {
        return None;
    }
    let body = &s[1..s.len() - 1];
    let mut out = String::new();
    let mut it = body.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            '0' => out.push('\0'),
            'x' => {
                let h: String = (0..2).filter_map(|_| it.next()).collect();
                out.push(char::from_u32(u32::from_str_radix(&h, 16).ok()?)?);
            }
            'u' => {
                let h: String = (0..4).filter_map(|_| it.next()).collect();
                out.push(char::from_u32(u32::from_str_radix(&h, 16).ok()?)?);
            }
            'U' => {
                let h: String = (0..8).filter_map(|_| it.next()).collect();
                out.push(char::from_u32(u32::from_str_radix(&h, 16).ok()?)?);
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspectable_rejects_calls() {
        for ok in ["df", "df.columns", "df['x']", "a[0]", "m.shape", "x_1"] {
            assert!(inspectable(ok), "{ok}");
        }
        for bad in ["f()", "df.head()", "__import__('os')", "a[f(1)]", "1+1", ""] {
            assert!(!inspectable(bad), "{bad}");
        }
    }

    #[test]
    fn python_literals_round_trip() {
        assert_eq!(
            python_str_literal("'a: int\\nb: str'").as_deref(),
            Some("a: int\nb: str")
        );
        assert_eq!(python_str_literal("\"it's\"").as_deref(), Some("it's"));
        assert_eq!(python_str_literal("'\\xe9\\u2192'").as_deref(), Some("é→"));
        assert_eq!(python_str_literal("42"), None);
        assert_eq!(python_repr("df['x']"), "'df[\\'x\\']'");
    }

    #[test]
    fn long_traceback_keeps_its_end() {
        let tb: Vec<Value> = (0..40).map(|i| Value::from(format!("line {i}"))).collect();
        let t = error_text(&serde_json::json!({"traceback": tb}), 25);
        assert!(t.starts_with("line 0") && t.contains("...") && t.ends_with("line 39"));
    }
}
