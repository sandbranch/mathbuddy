//! Connector for an R session running mathbuddy's bridge (RStudio, terminal
//! R). The bridge (r/bridge.R, built into this binary) runs inside the R
//! session and answers on 127.0.0.1 while R is idle, to requests carrying
//! its random token.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::session::{Cell, CellDone};

/// The bridge's R code, as shipped with this version.
pub const BRIDGE_R: &str = include_str!("../r/bridge.R");

/// Where bridges register themselves: $XDG_RUNTIME_DIR/mathbuddy.
pub fn bridge_dir() -> PathBuf {
    let d = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir());
    d.unwrap_or_else(std::env::temp_dir).join("mathbuddy")
}

/// Pre-check before the bridge's own (stricter, parse-based) check.
fn r_inspectable(expr: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"^[A-Za-z.][\w.]*(\$[A-Za-z.][\w.]*|\[\[?[^\[\]()]{1,40}\]?\]|@[A-Za-z.][\w.]*)*$",
        )
        .unwrap()
    })
    .is_match(expr)
}

#[derive(Clone)]
struct Endpoint {
    base: String,
    token: String,
}

impl Endpoint {
    /// One call to the bridge, on a blocking thread.
    async fn call(&self, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let (url, token) = (format!("{}{path}", self.base), self.token.clone());
        let out = tokio::task::spawn_blocking(move || {
            crate::http::json(
                &url,
                &[("X-Mathbuddy-Token", &token)],
                body.as_ref(),
                timeout,
            )
        })
        .await?
        .map_err(|e| {
            let s = e.to_string();
            if s.contains("did not answer in time") {
                anyhow!(
                    "R did not answer in {}s (is it busy running something?)",
                    timeout.as_secs()
                )
            } else {
                anyhow!("bridge: {s}")
            }
        })?;
        if let Some(err) = out["error"].as_str() {
            return Err(anyhow!("{err}"));
        }
        Ok(out)
    }
}

/// The project name from an idle R, for listing sessions (quick, may fail).
pub async fn probe_label(reg: &Value) -> Option<String> {
    let ep = Endpoint {
        base: format!("http://127.0.0.1:{}", reg["port"].as_u64()?),
        token: reg["token"].as_str()?.to_string(),
    };
    let info = ep
        .call("/info", None, Duration::from_millis(800))
        .await
        .ok()?;
    info["label"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

struct State {
    cells: Vec<Cell>,
    seq: u64,
    info: Value,
}

pub struct RBridge {
    pub cf: PathBuf,
    pub frontend: String,
    pid: u32,
    ep: Endpoint,
    state: Arc<Mutex<State>>,
    poll_task: tokio::task::AbortHandle,
}

impl Drop for RBridge {
    fn drop(&mut self) {
        self.poll_task.abort();
    }
}

impl RBridge {
    pub async fn connect(
        registry: &Path,
        done_tx: mpsc::UnboundedSender<CellDone>,
    ) -> Result<Self> {
        let reg: Value = serde_json::from_str(&std::fs::read_to_string(registry)?)?;
        let ep = Endpoint {
            base: format!(
                "http://127.0.0.1:{}",
                reg["port"].as_u64().ok_or_else(|| anyhow!("no port"))?
            ),
            token: reg["token"]
                .as_str()
                .ok_or_else(|| anyhow!("no token"))?
                .to_string(),
        };
        let state = Arc::new(Mutex::new(State {
            cells: Vec::new(),
            seq: 0,
            info: Value::Null,
        }));
        if let Ok(info) = ep.call("/info", None, Duration::from_secs(5)).await {
            state.lock().unwrap().info = info;
        }
        // what you did before mathbuddy arrived, without hints for old errors
        poll_events(&ep, &state, None).await?;
        let poll_task = {
            let (ep, state) = (ep.clone(), state.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let _ = poll_events(&ep, &state, Some(&done_tx)).await; // R busy: next tick
                }
            })
            .abort_handle()
        };
        Ok(RBridge {
            cf: registry.to_path_buf(),
            frontend: reg["frontend"].as_str().unwrap_or("R").into(),
            pid: reg["pid"].as_u64().unwrap_or(0) as u32,
            ep,
            state,
            poll_task,
        })
    }

    pub fn info(&self) -> Value {
        self.state.lock().unwrap().info.clone()
    }

    pub fn kernel_name(&self) -> String {
        let v = self.info()["r_version"].as_str().unwrap_or("").to_string();
        if v.is_empty() {
            format!("R ({})", self.frontend)
        } else {
            format!("R {v} ({})", self.frontend)
        }
    }

    pub fn recent(&self, n: usize) -> Vec<Cell> {
        let st = self.state.lock().unwrap();
        st.cells[st.cells.len().saturating_sub(n)..].to_vec()
    }

    pub async fn list_variables(&self) -> Result<String> {
        Ok(
            self.ep.call("/vars", None, Duration::from_secs(20)).await?["text"]
                .as_str()
                .unwrap_or("")
                .into(),
        )
    }

    pub async fn inspect(&self, expr: &str) -> Result<String> {
        let expr = expr.trim();
        if !r_inspectable(expr) {
            return Ok("refused: only names, $, [[ ]] and [ ] with constants can be inspected (no function calls)".into());
        }
        let out = self
            .ep
            .call(
                "/inspect",
                Some(json!({"expr": expr})),
                Duration::from_secs(20),
            )
            .await?;
        Ok(out["text"].as_str().unwrap_or("").into())
    }

    pub async fn run(&self, code: &str) -> Result<(String, String)> {
        let out = self
            .ep
            .call(
                "/run",
                Some(json!({"code": code})),
                Duration::from_secs(300),
            )
            .await?;
        let mut text = out["output"].as_str().unwrap_or("").to_string();
        let n = out["plots"].as_u64().unwrap_or(0);
        if n > 0 {
            let dev = out["device"]
                .as_str()
                .filter(|d| !d.is_empty())
                .unwrap_or("the active device");
            let place = if dev == "RStudioGD" {
                "the RStudio Plots pane"
            } else {
                dev
            };
            if !text.trim().is_empty() {
                text = format!("{}\n", text.trim_end());
            } else {
                text.clear();
            }
            text.push_str(&format!(
                "(drew {n} plot{} in {place})",
                if n > 1 { "s" } else { "" }
            ));
        }
        Ok((out["status"].as_str().unwrap_or("?").into(), text))
    }

    /// RStudio: "/insert" into the open script, or "/console".
    pub async fn editor(&self, path: &str, code: &str) -> Result<String> {
        let out = self
            .ep
            .call(path, Some(json!({"code": code})), Duration::from_secs(10))
            .await?;
        if out["status"] != "ok" {
            return Err(anyhow!("{}", out["output"].as_str().unwrap_or("failed")));
        }
        Ok(out["output"].as_str().unwrap_or("").into())
    }

    pub fn alive(&self) -> bool {
        crate::session::pid_alive(self.pid) && self.cf.exists()
    }
}

/// New commands since the last poll; errors go to `notify` (for hints).
async fn poll_events(
    ep: &Endpoint,
    state: &Arc<Mutex<State>>,
    notify: Option<&mpsc::UnboundedSender<CellDone>>,
) -> Result<()> {
    let since = state.lock().unwrap().seq;
    let out = ep
        .call(
            &format!("/events?since={since}"),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let mut st = state.lock().unwrap();
    for ev in out["events"].as_array().into_iter().flatten() {
        let seq = ev["seq"].as_u64().unwrap_or(0);
        let mut cell = Cell::new(
            format!("r-{seq}"),
            Some(seq as i64),
            ev["code"].as_str().unwrap_or("").into(),
        );
        cell.done = true;
        if let Some(v) = ev["value"].as_str().filter(|v| !v.is_empty()) {
            cell.outputs.push(v.into());
        }
        cell.error = ev["error"].as_str().map(String::from);
        st.seq = st.seq.max(seq);
        st.cells.push(cell.clone());
        let excess = st.cells.len().saturating_sub(crate::kernel::MAX_CELLS);
        st.cells.drain(..excess);
        if let Some(tx) = notify
            && cell.error.is_some()
        {
            let _ = tx.send(CellDone { cell });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_inspection_refuses_calls() {
        for ok in ["df", "df$x", "lst[['a']]", "m[1, 2]", "obj@slot", "my.var"] {
            assert!(r_inspectable(ok), "{ok}");
        }
        for bad in ["f(x)", "system('ls')", "df$x()", "1 + 1"] {
            assert!(!r_inspectable(bad), "{bad}");
        }
    }

    #[test]
    fn bridge_code_is_built_in() {
        assert!(BRIDGE_R.contains("X_MATHBUDDY_TOKEN") || BRIDGE_R.contains("MATHBUDDY_TOKEN"));
    }
}
