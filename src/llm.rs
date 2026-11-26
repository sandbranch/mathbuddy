//! Model servers: Ollama's /api/chat, and llama.cpp's llama-server
//! (OpenAI-style /v1/chat/completions, a single model or a router of
//! presets). Both stream; both do tool calls. Blocking: run on a blocking
//! thread.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

/// What streams back while the model answers.
pub enum Piece<'a> {
    Token(&'a str),
    Thinking(&'a str),
}

/// One assistant turn: its text and any tool calls, in Ollama's shape
/// (`{"id", "function": {"name", "arguments": {...}}}`).
#[derive(Debug, Default, Clone)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<Value>,
}

fn base_url(host: &str) -> String {
    let mut h = host.trim().to_string();
    if !h.contains("://") {
        h = format!("http://{h}");
    }
    if h.starts_with("http://0.0.0.0") {
        h = h.replacen("0.0.0.0", "127.0.0.1", 1);
    }
    h.trim_end_matches('/').to_string()
}

pub fn ollama_base(host: Option<&str>) -> String {
    let env = std::env::var("OLLAMA_HOST").ok();
    base_url(host.or(env.as_deref()).unwrap_or("127.0.0.1:11434"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerKind {
    Ollama,
    LlamaCpp,
}

impl ServerKind {
    pub fn name(self) -> &'static str {
        match self {
            ServerKind::Ollama => "ollama",
            ServerKind::LlamaCpp => "llama.cpp",
        }
    }
}

/// A model server and the options mathbuddy sends it.
#[derive(Clone, Debug)]
pub struct Server {
    pub kind: ServerKind,
    pub base: String,
    pub model: String,
    pub think: bool,
    /// Ollama only: keep the model off the graphics card, cap its threads,
    /// size its context window. llama.cpp runs as its preset says.
    pub cpu: bool,
    pub threads: Option<u32>,
    pub num_ctx: u32,
}

impl Server {
    pub fn ollama(
        host: Option<&str>,
        cpu: bool,
        threads: Option<u32>,
        think: bool,
        num_ctx: u32,
    ) -> Self {
        Server {
            kind: ServerKind::Ollama,
            base: ollama_base(host),
            model: String::new(),
            think,
            cpu,
            threads,
            num_ctx,
        }
    }

    pub fn llamacpp(host: &str, think: bool) -> Self {
        let mut base = base_url(host);
        if base.ends_with("/v1") {
            base.truncate(base.len() - 3);
        }
        Server {
            kind: ServerKind::LlamaCpp,
            base,
            model: String::new(),
            think,
            cpu: false,
            threads: None,
            num_ctx: 0,
        }
    }

    /// (name, loaded) for every model on the server. Listing loads nothing.
    pub fn catalog(&self) -> Result<Vec<(String, bool)>> {
        let t = Duration::from_secs(5);
        match self.kind {
            ServerKind::Ollama => {
                let tags = crate::http::json(&format!("{}/api/tags", self.base), &[], None, t)?;
                let loaded: Vec<String> =
                    crate::http::json(&format!("{}/api/ps", self.base), &[], None, t)
                        .ok()
                        .and_then(|v| v["models"].as_array().cloned())
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|m| m["name"].as_str().map(String::from))
                        .collect();
                Ok(tags["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["name"].as_str())
                    .map(|n| (n.to_string(), loaded.iter().any(|l| l == n)))
                    .collect())
            }
            ServerKind::LlamaCpp => {
                // a plain llama-server has one model, always loaded; a router
                // lists its presets with their status
                Ok(self
                    .llamacpp_models()?
                    .iter()
                    .filter_map(|m| {
                        let state = m["status"]["value"].as_str();
                        Some((
                            m["id"].as_str()?.to_string(),
                            state.is_none_or(|s| matches!(s, "loaded" | "loading" | "sleeping")),
                        ))
                    })
                    .collect())
            }
        }
    }

    fn llamacpp_models(&self) -> Result<Vec<Value>> {
        let v = crate::http::json(
            &format!("{}/v1/models", self.base),
            &[],
            None,
            Duration::from_secs(5),
        )?;
        Ok(v["data"].as_array().cloned().unwrap_or_default())
    }

    pub fn models(&self) -> Result<Vec<String>> {
        Ok(self.catalog()?.into_iter().map(|(n, _)| n).collect())
    }

    /// The server's name for `want`: Ollama's exact, `:latest` or a
    /// quantisation variant; llama.cpp's id or one of a preset's aliases.
    pub fn find(&self, want: &str) -> Result<Option<String>> {
        Ok(match self.kind {
            ServerKind::Ollama => find_model(&self.models()?, want),
            ServerKind::LlamaCpp => self.llamacpp_models()?.iter().find_map(|m| {
                let id = m["id"].as_str()?;
                let alias = m["aliases"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x == want));
                (id == want || alias).then(|| id.to_string())
            }),
        })
    }

    pub fn version(&self) -> Result<String> {
        match self.kind {
            ServerKind::Ollama => {
                let v = crate::http::json(
                    &format!("{}/api/version", self.base),
                    &[],
                    None,
                    Duration::from_secs(5),
                )?;
                Ok(v["version"].as_str().unwrap_or("?").to_string())
            }
            ServerKind::LlamaCpp => Ok("llama-server".into()),
        }
    }

    /// Stream one assistant turn.
    pub fn chat(
        &self,
        messages: &[Value],
        tools: Option<&Value>,
        max_tokens: Option<u32>,
        cancel: &AtomicBool,
        mut sink: impl FnMut(Piece),
    ) -> Result<Reply> {
        match self.kind {
            ServerKind::Ollama => self.chat_ollama(messages, tools, max_tokens, cancel, &mut sink),
            ServerKind::LlamaCpp => {
                self.chat_llamacpp(messages, tools, max_tokens, cancel, &mut sink)
            }
        }
    }

    fn chat_ollama(
        &self,
        messages: &[Value],
        tools: Option<&Value>,
        max_tokens: Option<u32>,
        cancel: &AtomicBool,
        sink: &mut impl FnMut(Piece),
    ) -> Result<Reply> {
        let mut options = json!({"num_ctx": self.num_ctx, "temperature": 0.3});
        if self.cpu {
            options["num_gpu"] = 0.into(); // keep every layer off the graphics card
        }
        if let Some(t) = self.threads {
            options["num_thread"] = t.into();
        }
        if let Some(n) = max_tokens {
            options["num_predict"] = n.into();
        }
        let mut body = json!({"model": self.model, "messages": messages, "stream": true, "think": self.think,
                              "keep_alive": "15m", "options": options});
        if let Some(t) = tools {
            body["tools"] = t.clone();
        }
        let mut reply = Reply::default();
        crate::http::stream_lines(&format!("{}/api/chat", self.base), &body, |line| {
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            if line.trim().is_empty() {
                return Ok(true);
            }
            let chunk: Value = serde_json::from_str(line)?;
            if let Some(e) = chunk["error"].as_str() {
                return Err(anyhow!("ollama: {e}"));
            }
            let msg = &chunk["message"];
            if let Some(t) = msg["thinking"].as_str().filter(|t| !t.is_empty()) {
                sink(Piece::Thinking(t));
            }
            if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
                reply.content.push_str(t);
                sink(Piece::Token(t));
            }
            reply
                .tool_calls
                .extend(msg["tool_calls"].as_array().cloned().unwrap_or_default());
            Ok(chunk["done"] != true)
        })
        .map_err(|e| anyhow!("ollama: {e}"))?;
        Ok(reply)
    }

    fn chat_llamacpp(
        &self,
        messages: &[Value],
        tools: Option<&Value>,
        max_tokens: Option<u32>,
        cancel: &AtomicBool,
        sink: &mut impl FnMut(Piece),
    ) -> Result<Reply> {
        // no sampling settings: the server's preset carries the model's own
        let mut body = json!({"model": self.model, "messages": openai_messages(messages), "stream": true,
                              "chat_template_kwargs": {"enable_thinking": self.think}});
        if let Some(n) = max_tokens {
            body["max_tokens"] = n.into();
        }
        if let Some(t) = tools {
            body["tools"] = t.clone();
        }
        let mut content = String::new();
        // per index: (id, name, arguments text), as the deltas build them up
        let mut calls: std::collections::BTreeMap<u64, (String, String, String)> =
            Default::default();
        crate::http::stream_lines(
            &format!("{}/v1/chat/completions", self.base),
            &body,
            |line| {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                    return Ok(true);
                };
                if data == "[DONE]" {
                    return Ok(false);
                }
                let chunk: Value = serde_json::from_str(data)?;
                if !chunk["error"].is_null() {
                    let e = &chunk["error"];
                    return Err(anyhow!(
                        "llama.cpp: {}",
                        e["message"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| e.to_string())
                    ));
                }
                for choice in chunk["choices"].as_array().into_iter().flatten() {
                    let d = &choice["delta"];
                    if let Some(t) = d["reasoning_content"].as_str().filter(|t| !t.is_empty()) {
                        sink(Piece::Thinking(t));
                    }
                    if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
                        content.push_str(t);
                        sink(Piece::Token(t));
                    }
                    for tc in d["tool_calls"].as_array().into_iter().flatten() {
                        let next = calls.len() as u64;
                        let c = calls
                            .entry(tc["index"].as_u64().unwrap_or(next))
                            .or_default();
                        if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                            c.0 = id.into();
                        }
                        c.1.push_str(tc["function"]["name"].as_str().unwrap_or(""));
                        c.2.push_str(tc["function"]["arguments"].as_str().unwrap_or(""));
                    }
                }
                Ok(true)
            },
        )
        .map_err(|e| {
            if e.to_string().starts_with("llama.cpp") {
                e
            } else {
                anyhow!("llama.cpp: {e}")
            }
        })?;
        let tool_calls = calls
            .into_values()
            .map(|(id, name, args)| {
                let args = if args.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&args).unwrap_or(Value::String(args))
                };
                json!({"id": id, "function": {"name": name, "arguments": args}})
            })
            .collect();
        Ok(Reply {
            content,
            tool_calls,
        })
    }
}

/// mathbuddy's messages (Ollama style) in OpenAI form: tool-call arguments
/// as JSON text, every call with an id, each tool result tied to its call.
pub fn openai_messages(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pending: std::collections::VecDeque<String> = Default::default();
    let mut n = 0;
    for m in messages {
        match m["role"].as_str() {
            Some("assistant") if m["tool_calls"].as_array().is_some_and(|a| !a.is_empty()) => {
                let calls: Vec<Value> = m["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| {
                        let f = &c["function"];
                        let args = match &f["arguments"] {
                            Value::String(s) => s.clone(),
                            Value::Null => "{}".into(),
                            v => v.to_string(),
                        };
                        n += 1;
                        let id = c["id"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or(format!("call_{n}"));
                        pending.push_back(id.clone());
                        json!({"id": id, "type": "function", "function": {"name": f["name"], "arguments": args}})
                    })
                    .collect();
                out.push(json!({"role": "assistant", "content": m["content"].as_str().unwrap_or(""), "tool_calls": calls}));
            }
            Some("tool") => {
                n += 1;
                let id = pending.pop_front().unwrap_or(format!("call_{n}"));
                out.push(json!({"role": "tool", "tool_call_id": id, "content": m["content"]}));
            }
            _ => out.push(json!({"role": m["role"], "content": m["content"]})),
        }
    }
    out
}

/// The server's name for `want`: exact, `want:latest`, or a quantisation
/// variant like qwen3.5:4b-q4_K_M for qwen3.5:4b.
pub fn find_model(names: &[String], want: &str) -> Option<String> {
    names
        .iter()
        .find(|n| {
            *n == want
                || **n == format!("{want}:latest")
                || (want.contains(':') && n.starts_with(&format!("{want}-")))
        })
        .cloned()
}

/// Tool-calling models, best first, used when no model is given. Tested on
/// 2026-11-10: qwen3.5 (9b) reliable; qwen3.5:4b usually right, sometimes
/// assumes values; qwen3:4b only works with thinking on (slow); qwen3.5:2b
/// writes broken R and calls functions that don't exist, so it isn't listed.
pub const PREFERRED_MODELS: [&str; 7] = [
    "qwen3.5:latest",
    "qwen3.5:9b",
    "gemma4",
    "qwen3:8b",
    "qwen3.5:4b",
    "qwen3:4b",
    "gpt-oss:20b",
];

/// Hybrid thinking models whose "think: false" Ollama 0.34 doesn't honour:
/// with it off they reason in the answer and fumble tool calls.
pub const THINK_ALWAYS: [&str; 1] = ["qwen3:"];

/// The model in use, on one of the configured servers.
pub struct Models {
    pub servers: Vec<Server>,
    pub current: usize,
}

impl Models {
    pub fn server(&self) -> &Server {
        &self.servers[self.current]
    }

    pub fn use_model(&mut self, server: usize, name: &str) {
        self.current = server;
        self.servers[server].model = name.to_string();
        if !self.servers[server].think && THINK_ALWAYS.iter().any(|p| name.starts_with(p)) {
            self.servers[server].think = true; // hidden unless --think; see THINK_ALWAYS
        }
    }

    pub fn label(&self) -> String {
        let s = self.server();
        if s.cpu {
            format!("{} (CPU)", s.model)
        } else if s.kind == ServerKind::LlamaCpp {
            format!("{} (llama.cpp)", s.model)
        } else {
            s.model.clone()
        }
    }

    /// (server index, name) for a model name on any server, Ollama first.
    pub fn locate(&self, want: &str) -> Option<(usize, String)> {
        self.servers
            .iter()
            .enumerate()
            .find_map(|(i, s)| s.find(want).ok().flatten().map(|n| (i, n)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_model_variants() {
        let names: Vec<String> = ["qwen3.5:latest", "qwen3.5:4b-q4_K_M", "gpt-oss:20b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            find_model(&names, "qwen3.5").as_deref(),
            Some("qwen3.5:latest")
        );
        assert_eq!(
            find_model(&names, "qwen3.5:4b").as_deref(),
            Some("qwen3.5:4b-q4_K_M")
        );
        assert_eq!(
            find_model(&names, "gpt-oss:20b").as_deref(),
            Some("gpt-oss:20b")
        );
        assert_eq!(find_model(&names, "gemma4"), None);
    }

    #[test]
    fn tool_results_tied_to_calls() {
        let msgs = openai_messages(&[
            json!({"role": "system", "content": "s"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "function": {"name": "inspect", "arguments": {"expr": "df"}}},
                {"function": {"name": "run_code", "arguments": {"code": "1"}}}]}),
            json!({"role": "tool", "tool_name": "inspect", "content": "df: 6 rows"}),
            json!({"role": "tool", "tool_name": "run_code", "content": "1"}),
        ]);
        let calls = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["id"], "a");
        assert_eq!(
            serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap())
                .unwrap(),
            json!({"expr": "df"})
        );
        assert_eq!(msgs[2]["tool_call_id"], "a");
        assert_eq!(msgs[3]["tool_call_id"], calls[1]["id"]);
        assert!(msgs[2].get("tool_name").is_none());
    }

    #[test]
    fn hosts() {
        assert_eq!(base_url("0.0.0.0:11434"), "http://127.0.0.1:11434");
        assert_eq!(
            Server::llamacpp("127.0.0.1:8081/v1", false).base,
            "http://127.0.0.1:8081"
        );
    }
}
