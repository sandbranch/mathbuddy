//! Against real kernels and a real R: ipykernel, giac-kernel and an
//! interactive R with the bridge. The model is a fake Ollama with scripted
//! answers, so no model or GPU is needed.

mod common;

use std::{
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

use common::*;
use mathbuddy::{
    assistant::{Assistant, Confirm, Event},
    kernel::KernelWatch,
    llm::Server,
    rbridge::RBridge,
    session::{CellDone, Connection},
};
use serde_json::json;
use tokio::sync::mpsc;

#[tokio::test(flavor = "multi_thread")]
async fn python_watch_inspect_run() {
    let k = python_kernel().await;
    let mut fe = Frontend::new(&k.info).await;
    // run before attaching: seeded from history
    fe.run("import pandas as pd\nxs = [1, 2, 3, 4]").await;
    let (tx, mut done_rx) = mpsc::unbounded_channel::<CellDone>();
    let w = KernelWatch::connect(&k.cf, tx).await.unwrap();
    assert_eq!(w.language, "python");
    assert!(
        w.recent(10)
            .iter()
            .any(|c| c.code.contains("xs = [1, 2, 3, 4]")),
        "history seeded"
    );

    // the user runs a cell: seen with its output
    fe.run("df = pd.DataFrame({'g': ['AAA', 'BBB'] * 3, 'y': [4.2, 5.1, 3.9, 5.6, 4.4, 5.0]})\nprint(len(df))").await;
    wait_for(10, "cell seen", || {
        w.recent(10)
            .iter()
            .any(|c| c.code.contains("pd.DataFrame") && c.done)
    })
    .await;
    let cell = w
        .recent(10)
        .into_iter()
        .find(|c| c.code.contains("pd.DataFrame"))
        .unwrap();
    assert_eq!(cell.outputs.concat().trim(), "6");

    // looking without running anything visible
    let vars = w.list_variables().await.unwrap();
    assert!(
        vars.contains("df: DataFrame shape=(6, 2)") && vars.contains("xs: list len=4"),
        "{vars}"
    );
    let d = w.inspect("df").await.unwrap();
    assert!(d.contains("shape=(6, 2)") && d.contains("dtypes:"), "{d}");
    assert!(w.inspect("df.head()").await.unwrap().starts_with("refused"));
    assert!(
        !w.recent(20).iter().any(|c| c.code.contains("_mathbuddy")),
        "helper calls stay invisible"
    );

    // running on the user's say-so: output comes back, variables stay
    let (status, out) = w.run("m = sum(xs) / len(xs)\nprint(m)").await.unwrap();
    assert_eq!((status.as_str(), out.trim()), ("ok", "2.5"));
    assert!(w.inspect("m").await.unwrap().contains("repr: 2.5"));
    let (status, out) = w.run("1/0").await.unwrap();
    assert_eq!(status, "error");
    assert!(out.contains("ZeroDivisionError"), "{out}");

    // the user's error comes through for a hint
    fe.run("undefined_name + 1").await;
    let done = tokio::time::timeout(Duration::from_secs(10), done_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(done.cell.error.unwrap().contains("NameError"));
}

#[tokio::test(flavor = "multi_thread")]
async fn giac_inspection_through_user_expressions() {
    let kernel = std::env::var("GIAC_KERNEL").unwrap_or_else(|_| "/usr/bin/giac-kernel".into());
    if !std::path::Path::new(&kernel).exists() {
        eprintln!("skipped: no giac-kernel at {kernel}");
        return;
    }
    let k = start_kernel("giac", &[&kernel, "{connection_file}"]).await;
    let mut fe = Frontend::new(&k.info).await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let w = KernelWatch::connect(&k.cf, tx).await.unwrap();
    assert_eq!(w.language, "giac");
    fe.run("a:=3:;").await;
    fe.run("p:=x^2-1:;").await;
    let vars = w.list_variables().await.unwrap();
    if vars.contains("TypeError") || vars.contains("no result") || vars == "(no user variables)" {
        eprintln!(
            "skipped: {kernel} doesn't evaluate user_expressions (needs giac-kernel 9d7fff4 or later): {vars}"
        );
        return;
    }
    assert!(vars.contains('a') && vars.contains('p'), "{vars}");
    let a = w.inspect("a").await.unwrap();
    assert!(a.contains("value: 3"), "{a}");
    let (status, out) = w.run("factor(p)").await.unwrap();
    assert_eq!(status, "ok");
    assert!(out.contains("(x-1)*(x+1)"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn r_bridge_vars_inspect_run_events() {
    let bridge = std::env::temp_dir().join(format!("mathbuddy-bridge-{}.R", uuid::Uuid::new_v4()));
    std::fs::write(&bridge, mathbuddy::rbridge::BRIDGE_R).unwrap();
    let mut r = RSession::start(&bridge);
    r.send("AAA <- c(4.2, 3.9, 4.4); BBB <- c(5.1, 5.6, 5.0)");
    let reg = r.registry().await;
    let (tx, mut done_rx) = mpsc::unbounded_channel::<CellDone>();
    let b = RBridge::connect(&reg, tx).await.unwrap();
    let vars = b.list_variables().await.unwrap();
    assert!(vars.contains("AAA") && vars.contains("BBB"), "{vars}");
    let ins = b.inspect("AAA").await.unwrap();
    assert!(ins.contains("4.2"), "{ins}");
    assert!(
        b.inspect("system('ls')")
            .await
            .unwrap()
            .starts_with("refused")
    );
    let (status, out) = b.run("print(mean(AAA))").await.unwrap();
    assert_eq!(status, "ok");
    assert!(out.contains("4.16"), "{out}");
    // the user's commands arrive as events; errors for hints
    r.send("CCC <- AAA + 1");
    r.send("stop(\"boom\")");
    let done = tokio::time::timeout(Duration::from_secs(15), done_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(done.cell.error.unwrap().contains("boom"));
    assert!(
        b.recent(10)
            .iter()
            .any(|c| c.code.contains("CCC <- AAA + 1"))
    );
    let _ = std::fs::remove_file(&bridge);
}

#[tokio::test(flavor = "multi_thread")]
async fn assistant_tool_loop_runs_approved_code() {
    let k = python_kernel().await;
    let mut fe = Frontend::new(&k.info).await;
    fe.run("xs = [1, 2, 3, 4]").await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let conn = Connection::Kernel(KernelWatch::connect(&k.cf, tx).await.unwrap());

    let call = |name: &str, args: serde_json::Value| {
        vec![
            json!({"message": {"content": "", "tool_calls": [{"function": {"name": name, "arguments": args}}]}, "done": true}),
        ]
    };
    let ollama = FakeOllama::start(
        &["qwen3.5:latest"],
        vec![
            call("list_variables", json!({})),
            call(
                "run_code",
                json!({"code": "m = sum(xs) / len(xs)\nprint(m)"}),
            ),
            vec![
                json!({"message": {"content": "The mean of xs "}, "done": false}),
                json!({"message": {"content": "is 2.5."}, "done": true}),
            ],
        ],
    );
    let mut server = Server::ollama(Some(&ollama.host), false, None, false, 8192);
    server.model = "qwen3.5:latest".into();

    let mut a = Assistant::default();
    let (etx, mut erx) = mpsc::unbounded_channel();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let confirm: Confirm = Box::new(move |code: String| {
        seen.lock().unwrap().push(code);
        Box::pin(async { true })
    });
    let answer = a
        .ask(
            &server,
            Some(&conn),
            "what is the mean of xs?",
            etx,
            confirm,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
    assert_eq!(answer, "The mean of xs is 2.5.");
    assert_eq!(
        asked.lock().unwrap().len(),
        1,
        "every run is approved first"
    );
    assert_eq!(a.runs, vec!["m = sum(xs) / len(xs)\nprint(m)".to_string()]);
    assert!(
        conn.inspect("m").await.unwrap().contains("repr: 2.5"),
        "the run happened in the user's kernel"
    );

    let mut results = Vec::new();
    while let Ok(ev) = erx.try_recv() {
        if let Event::ToolResult(r) = ev {
            results.push(r);
        }
    }
    assert!(results[0].contains("xs: list len=4"), "{results:?}");
    assert_eq!(results[1].trim(), "status: ok\n2.5");

    // the model saw the tool results, and the session in its system prompt
    let reqs = ollama.requests.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    assert!(
        reqs[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("xs = [1, 2, 3, 4]")
    );
    let last = reqs[2]["messages"].as_array().unwrap();
    assert!(
        last.iter()
            .any(|m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("2.5"))
    );
    assert_eq!(reqs[0]["tools"].as_array().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn declined_run_is_reported_to_the_model() {
    let k = python_kernel().await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let conn = Connection::Kernel(KernelWatch::connect(&k.cf, tx).await.unwrap());
    let ollama = FakeOllama::start(
        &["qwen3.5:latest"],
        vec![
            vec![
                json!({"message": {"content": "", "tool_calls": [{"function": {"name": "run_code",
                "arguments": {"code": "z = 41 + 1"}}}]}, "done": true}),
            ],
            vec![
                json!({"message": {"content": "Run this:\n```python\nz = 41 + 1\n```"}, "done": true}),
            ],
        ],
    );
    let mut server = Server::ollama(Some(&ollama.host), false, None, false, 8192);
    server.model = "qwen3.5:latest".into();
    let mut a = Assistant::default();
    let (etx, _erx) = mpsc::unbounded_channel();
    let answer = a
        .ask(
            &server,
            Some(&conn),
            "make z",
            etx,
            Box::new(|_c: String| Box::pin(async { false })),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
    assert!(answer.contains("```python"));
    assert!(a.runs.is_empty());
    assert!(
        conn.inspect("z").await.unwrap().contains("NameError"),
        "nothing ran"
    );
    let reqs = ollama.requests.lock().unwrap();
    let tool = reqs[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()
        .clone();
    assert!(tool["content"].as_str().unwrap().contains("declined"));
}

/// The llama.cpp path: OpenAI-style SSE from a llama-server router, with the
/// thinking, the tool calls arriving in pieces, and the answer.
#[tokio::test(flavor = "multi_thread")]
async fn llamacpp_router_tool_loop() {
    let k = python_kernel().await;
    let mut fe = Frontend::new(&k.info).await;
    fe.run("ys = [2, 4, 9]").await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let conn = Connection::Kernel(KernelWatch::connect(&k.cf, tx).await.unwrap());

    let delta = |d: serde_json::Value| json!({"choices": [{"index": 0, "delta": d}]});
    let fake = FakeOllama::start(
        &["bonsai-27b", "other-model"],
        vec![
            // a call whose name and arguments come in three pieces
            vec![
                delta(json!({"reasoning_content": "Sum the list. "})),
                delta(
                    json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "run_code", "arguments": ""}}]}),
                ),
                delta(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"code\": \"t = sum(ys)"}}]}),
                ),
                delta(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "\\nprint(t)\"}"}}]}),
                ),
            ],
            vec![
                delta(json!({"content": "The total "})),
                delta(json!({"content": "is 15."})),
            ],
        ],
    );
    let mut server = Server::llamacpp(&fake.host, false);
    assert_eq!(
        server.catalog().unwrap(),
        vec![("bonsai-27b".into(), true), ("other-model".into(), false)]
    );
    assert_eq!(
        server.find("bonsai-27b").unwrap().as_deref(),
        Some("bonsai-27b")
    );
    server.model = "bonsai-27b".into();

    let mut a = Assistant::default();
    let (etx, mut erx) = mpsc::unbounded_channel();
    let answer = a
        .ask(
            &server,
            Some(&conn),
            "what's the total of ys?",
            etx,
            Box::new(|_c: String| Box::pin(async { true })),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
    assert_eq!(answer, "The total is 15.");
    assert_eq!(a.runs, vec!["t = sum(ys)\nprint(t)".to_string()]);
    assert!(conn.inspect("t").await.unwrap().contains("repr: 15"));
    let mut thought = String::new();
    while let Ok(ev) = erx.try_recv() {
        if let Event::Thinking(t) = ev {
            thought.push_str(&t);
        }
    }
    assert_eq!(thought, "Sum the list. ");

    let reqs = fake.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    let first = &reqs[0];
    assert_eq!(first["model"], "bonsai-27b");
    assert_eq!(first["stream"], true);
    assert_eq!(first["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(first["tools"].as_array().unwrap().len(), 3);
    for knob in ["temperature", "top_p", "top_k", "min_p"] {
        assert!(first.get(knob).is_none(), "{knob} is the preset's business");
    }
    // the second request carries the call (arguments as JSON text) and its
    // result tied to the call's id
    let msgs = reqs[1]["messages"].as_array().unwrap();
    let call = msgs
        .iter()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .unwrap();
    let tc = &call["tool_calls"][0];
    assert_eq!(tc["id"], "call_1");
    assert_eq!(tc["function"]["name"], "run_code");
    let args: serde_json::Value =
        serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["code"], "t = sum(ys)\nprint(t)");
    let result = msgs.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(result["tool_call_id"], "call_1");
    assert!(result["content"].as_str().unwrap().contains("15"));
}
