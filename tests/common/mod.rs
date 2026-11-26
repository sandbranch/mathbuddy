//! Real kernels and R for the integration tests, a second "frontend" that
//! runs cells the way jotter would, and a fake Ollama with scripted answers.
#![allow(dead_code)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use jupyter_protocol::{
    ConnectionInfo, Transport,
    messaging::{ExecuteRequest, JupyterMessage},
};

/// A scratch directory removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("mathbuddy-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A kernel process with its connection file, killed when dropped.
pub struct Kernel {
    pub child: Child,
    pub cf: PathBuf,
    pub info: ConnectionInfo,
    pub _dir: TempDir,
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn connection_file(dir: &Path, name: &str) -> (PathBuf, ConnectionInfo) {
    let ip = "127.0.0.1".parse().unwrap();
    let ports = jupyter_zmq_client::peek_ports(ip, 5).await.unwrap();
    let info = ConnectionInfo {
        ip: "127.0.0.1".into(),
        transport: Transport::TCP,
        shell_port: ports[0],
        iopub_port: ports[1],
        stdin_port: ports[2],
        control_port: ports[3],
        hb_port: ports[4],
        key: uuid::Uuid::new_v4().to_string(),
        signature_scheme: "hmac-sha256".into(),
        kernel_name: Some(name.into()),
    };
    let cf = dir.join(format!("kernel-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&cf, serde_json::to_string(&info).unwrap()).unwrap();
    (cf, info)
}

/// Start a kernel (`argv` with `{connection_file}`) and wait for its heartbeat.
pub async fn start_kernel(name: &str, argv: &[&str]) -> Kernel {
    let dir = TempDir::new(name);
    let (cf, info) = connection_file(&dir.0, name).await;
    let args: Vec<String> = argv
        .iter()
        .map(|a| a.replace("{connection_file}", cf.to_str().unwrap()))
        .collect();
    let child = Command::new(&args[0])
        .args(&args[1..])
        .env("IPYTHONDIR", dir.0.join("ipython")) // test kernels stay out of ~/.ipython
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("starting {}: {e}", args[0]));
    let k = Kernel {
        child,
        cf,
        info,
        _dir: dir,
    };
    let t0 = Instant::now();
    while !mathbuddy::kernel::kernel_alive(&k.cf, Duration::from_millis(500)).await {
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "kernel {name} did not come up"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    k
}

pub async fn python_kernel() -> Kernel {
    start_kernel(
        "python3",
        &[
            "python3",
            "-m",
            "ipykernel_launcher",
            "-f",
            "{connection_file}",
        ],
    )
    .await
}

/// Another client on the kernel, running cells as the user's frontend does.
pub struct Frontend {
    shell: jupyter_zmq_client::ClientShellConnection,
    session: String,
}

impl Frontend {
    pub async fn new(info: &ConnectionInfo) -> Self {
        let session = uuid::Uuid::new_v4().to_string();
        let identity = jupyter_zmq_client::peer_identity_for_session(&session).unwrap();
        let shell = jupyter_zmq_client::create_client_shell_connection_with_identity(
            info, &session, identity,
        )
        .await
        .unwrap();
        Frontend { shell, session }
    }

    /// Run a cell (stored in history, broadcast on IOPub) and wait for its reply.
    pub async fn run(&mut self, code: &str) {
        let msg = JupyterMessage::new(
            ExecuteRequest {
                code: code.into(),
                silent: false,
                store_history: true,
                user_expressions: None,
                allow_stdin: false,
                stop_on_error: false,
            },
            None,
        )
        .with_session(&self.session);
        let id = msg.header.msg_id.clone();
        self.shell.send(msg).await.unwrap();
        loop {
            let r = tokio::time::timeout(Duration::from_secs(60), self.shell.read())
                .await
                .unwrap()
                .unwrap();
            if r.parent_header.as_ref().is_some_and(|h| h.msg_id == id) {
                return;
            }
        }
    }
}

/// Wait until `f` holds, or fail after `secs`.
pub async fn wait_for(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(
            t0.elapsed() < Duration::from_secs(secs),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A fake model server, Ollama and llama-server at once: /api/tags and
/// /v1/models list `models` (the router listing has the first one loaded).
/// Each POST /api/chat streams the next scripted answer as NDJSON lines,
/// each POST /v1/chat/completions as SSE events; both record the request.
pub struct FakeOllama {
    pub host: String,
    pub requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl FakeOllama {
    pub fn start(models: &[&str], answers: Vec<Vec<serde_json::Value>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let answers = Arc::new(Mutex::new(std::collections::VecDeque::from(answers)));
        let tags = serde_json::json!({"models": models.iter().map(|m| serde_json::json!({"name": m})).collect::<Vec<_>>()});
        // the same models as a llama-server router lists them: the first loaded
        let v1_models = serde_json::json!({"object": "list", "data": models.iter().enumerate().map(|(i, m)| serde_json::json!({
            "id": m, "object": "model", "status": {"value": if i == 0 { "loaded" } else { "unloaded" }}})).collect::<Vec<_>>()});
        let reqs = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let mut out = stream;
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let payload = match path.as_str() {
                    "/api/tags" => tags.to_string(),
                    "/api/ps" => r#"{"models": []}"#.to_string(),
                    "/api/version" => r#"{"version": "0.0-fake"}"#.to_string(),
                    "/v1/models" => v1_models.to_string(),
                    "/v1/chat/completions" => {
                        reqs.lock()
                            .unwrap()
                            .push(serde_json::from_slice(&body).unwrap_or_default());
                        let lines = answers.lock().unwrap().pop_front().unwrap_or_default();
                        let mut sse: String =
                            lines.iter().map(|l| format!("data: {l}\n\n")).collect();
                        sse.push_str("data: [DONE]\n\n");
                        sse
                    }
                    "/api/chat" => {
                        reqs.lock()
                            .unwrap()
                            .push(serde_json::from_slice(&body).unwrap_or_default());
                        let lines = answers.lock().unwrap().pop_front().unwrap_or_else(|| {
                            vec![serde_json::json!({"message": {"content": "(no more scripted answers)"}, "done": true})]
                        });
                        lines.iter().map(|l| l.to_string() + "\n").collect()
                    }
                    _ => "{}".into(),
                };
                let _ = write!(
                    out,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        FakeOllama { host, requests }
    }
}

/// An interactive R fed through a pipe, as a terminal R session would be.
pub struct RSession {
    pub child: Child,
    pub dir: TempDir,
}

impl RSession {
    pub fn start(bridge: &Path) -> Self {
        let dir = TempDir::new("r");
        let mut child = Command::new("R")
            .args([
                "--interactive",
                "--no-readline",
                "--no-save",
                "--quiet",
                "--no-init-file",
            ])
            .env("XDG_RUNTIME_DIR", &dir.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting R");
        writeln!(
            child.stdin.as_mut().unwrap(),
            "source(\"{}\")",
            bridge.display()
        )
        .unwrap();
        RSession { child, dir }
    }

    pub fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    /// The bridge's registry file, once R has written it.
    pub async fn registry(&self) -> PathBuf {
        let d = self.dir.0.join("mathbuddy");
        let t0 = Instant::now();
        loop {
            if let Some(f) = std::fs::read_dir(&d).ok().and_then(|rd| {
                rd.flatten().map(|e| e.path()).find(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("r-"))
                })
            }) {
                return f;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(30),
                "R bridge did not register"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for RSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
