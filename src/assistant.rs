//! What the model sees, the tools it may use, and the answer loop.

use std::sync::{Arc, atomic::AtomicBool};

use anyhow::Result;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    llm::{Piece, Server},
    session::{Cell, Connection},
};

const SYSTEM: &str = "You are mathbuddy, a small assistant looking over the user's shoulder \
while they work in a live {language} session in {frontend}. You can see what \
they run, as they run it, below.

How to help:
- Be brief. One short paragraph or a short list, then code if code is wanted.
- Check before guessing: use list_variables and inspect to see what a variable \
really is (columns, dtype, shape) before writing code against it.
- To compute an answer, call run_code. The user approves each run. Every \
number you report must come from run_code output you actually received in \
this conversation. Never estimate or invent a result.
- Never invent data or parameters (an SD, a sample size, a constant you were \
not given): compute them from the user's variables, or ask.
- After run_code succeeds, do NOT repeat its code in your answer: the user \
already saw it and can /insert or /copy it. Answer in one or two sentences: \
what it did and the result.
- If run_code is declined or not available, give ONE fenced code block that \
runs as-is in their session (using their existing variable names) and say \
they can run it with /run.
- For data entry (a pasted table, a list of values), turn it into the obvious \
structure (a data frame, an array) with sensible column names and units.
- If something is ambiguous or you are unsure, say so in one line.
{lang_hints}
{session}";

const R_HINTS: &str = r#"
R specifics:
- Write R, in ```r blocks. Plots you draw go to their active graphics device (in RStudio, the Plots pane), so draw the plot itself; don't save it to a file unless asked.
- Installed helper packages: {packages}. Prefer ggplot2 for plots when it is installed (library(ggplot2) first), unless they ask for base graphics.
- Error bars: summarise first (mean and sd or se per group, computed from the data), then geom_col() + geom_errorbar(aes(ymin = ..., ymax = ...)), and say which spread they show (SD, SE or CI).
- One colour for all bars: set it outside aes(), e.g. geom_col(fill = "red"). NEVER write aes(fill = "red"): a constant inside aes() does not colour the bars red, it makes a legend entry named "red" in ggplot's default colour. Map fill or colour to a variable only when the colour should mean something.
- Label the axes with what they measure, units if known.
- print() the numbers you need to see; a ggplot object is drawn by printing it.
- Load every package you use: %>%, group_by() and summarise() need library(dplyr) first. Base R needs nothing, so prefer it for small summaries.
- Building one data frame from several vectors: make the group column with rep(), e.g. rep(c("AAA", "BBB"), times = c(length(AAA), length(BBB))). Never put c("AAA", "BBB") next to longer values: R silently recycles it and mixes up the groups.
- geom_col() draws one bar per row and stacks rows that share an x value, so summarise to exactly one row per group first (as in the example), or the bars come out as sums, not means.
- Example, bar chart of two vectors with ±1 SD error bars and one colour:
```r
library(ggplot2)
d <- data.frame(group = c("AAA", "BBB"),
                mean = c(mean(AAA), mean(BBB)),
                sd = c(sd(AAA), sd(BBB)))
ggplot(d, aes(x = group, y = mean)) +
  geom_col(fill = "red") +
  geom_errorbar(aes(ymin = mean - sd, ymax = mean + sd), width = 0.2) +
  labs(x = NULL, y = "Mean (±1 SD)")
```
"#;

const PYTHON_HINTS: &str =
    "\nPython specifics: write Python, in ```python blocks; pandas and numpy for data.\n";

const GIAC_HINTS: &str = "\nGiac specifics: write Giac (Xcas) commands, in ```giac blocks: := assigns, \
and functions like factor(), solve(), int(), diff() and plot() do the work. A trailing :; \
hides a result. list_variables shows what is assigned (VARS()).\n";

fn tools() -> Value {
    json!([
        {"type": "function", "function": {
            "name": "list_variables",
            "description": "List the user's variables in the live session, with type and shape.",
            "parameters": {"type": "object", "properties": {}}}},
        {"type": "function", "function": {
            "name": "inspect",
            "description": "Describe one variable in the live session: type, shape or dim, columns, first rows, \
    summary. Accepts a name, attribute access or a simple subscript (Python: df['x'], df.columns; R: df$x, \
    lst[['a']]), but no function calls.",
            "parameters": {"type": "object",
                "properties": {"expr": {"type": "string", "description": "e.g. df, df.columns, df$x"}},
                "required": ["expr"]}}},
        {"type": "function", "function": {
            "name": "run_code",
            "description": "Run code, in the session's language, in the user's live session and get its printed \
    output back. The user is asked to approve every run. Variables it creates stay in their session, and plots \
    appear in their plot pane. print() what you need to see.",
            "parameters": {"type": "object",
                "properties": {"code": {"type": "string", "description": "code to run"}},
                "required": ["code"]}}}
    ])
}

const TOOL_NAMES: &str = "list_variables, inspect, run_code";

fn session_block(conn: Option<&Connection>, n: usize) -> String {
    let Some(c) = conn else {
        return "No session is attached yet, so you cannot see their work.".into();
    };
    let cells = c.recent(n);
    if cells.is_empty() {
        return format!(
            "Attached to a {} session; nothing has been run since attaching.",
            c.language()
        );
    }
    let body: Vec<String> = cells.iter().map(|c| c.render(1500, 800)).collect();
    format!(
        "What they ran most recently, oldest first:\n\n{}",
        body.join("\n\n")
    )
}

/// Fenced code blocks in an answer, in order.
pub fn code_blocks(text: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?s)```[ \t]*[\w+-]*[ \t]*\n(.*?)```").unwrap())
        .captures_iter(text)
        .map(|c| c[1].trim_end().to_string())
        .collect()
}

/// Asked before every run_code: the code, then true to run it.
pub type Confirm = Box<dyn FnMut(String) -> BoxFuture<'static, bool> + Send>;

/// What the answer loop reports while it works.
#[derive(Debug, Clone)]
pub enum Event {
    Token(String),
    Thinking(String),
    /// A tool the model called, with its argument (an expression), if any.
    Tool {
        name: String,
        arg: String,
    },
    /// A tool's result, as the model sees it.
    ToolResult(String),
}

pub struct Assistant {
    pub history: Vec<Value>,
    /// Code that ran successfully during the last ask().
    pub runs: Vec<String>,
    max_history: usize,
    max_rounds: usize,
}

impl Default for Assistant {
    fn default() -> Self {
        Assistant {
            history: Vec::new(),
            runs: Vec::new(),
            max_history: 20,
            max_rounds: 6,
        }
    }
}

impl Assistant {
    fn system(&self, conn: Option<&Connection>) -> Value {
        let lang = conn
            .map(|c| c.language())
            .unwrap_or_else(|| "Python".into());
        let frontend = conn
            .map(|c| c.frontend())
            .unwrap_or_else(|| "a Jupyter frontend (jotter)".into());
        let hints = match lang.as_str() {
            "R" => {
                let pk: Vec<String> = conn
                    .map(|c| c.info()["packages"].as_array().cloned().unwrap_or_default())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|p| p.as_str().map(String::from))
                    .collect();
                let pk = if pk.is_empty() {
                    "none of ggplot2/dplyr/tidyr/data.table".into()
                } else {
                    pk.join(", ")
                };
                R_HINTS.replace("{packages}", &pk)
            }
            "python" => PYTHON_HINTS.into(),
            "giac" => GIAC_HINTS.into(),
            _ => String::new(),
        };
        let content = SYSTEM
            .replace("{language}", &lang)
            .replace("{frontend}", &frontend)
            .replace("{lang_hints}", &hints)
            .replace("{session}", &session_block(conn, 8));
        json!({"role": "system", "content": content})
    }

    async fn tool(
        &mut self,
        conn: Option<&Connection>,
        name: &str,
        args: &Value,
        confirm: &mut Confirm,
    ) -> String {
        let Some(c) = conn else {
            return "no session attached".into();
        };
        let r: Result<String> = async {
            Ok(match name {
                "list_variables" => c.list_variables().await?,
                "inspect" => c.inspect(args["expr"].as_str().unwrap_or("")).await?,
                "run_code" => {
                    let code = args["code"].as_str().unwrap_or("").trim().to_string();
                    if code.is_empty() {
                        return Ok("no code given".into());
                    }
                    if !confirm(code.clone()).await {
                        return Ok(
                            "The user declined this run. Do not report a computed result. \
Give the code as a fenced block they can /run themselves."
                                .into(),
                        );
                    }
                    let (status, out) = c.run(&code).await?;
                    if status == "ok" {
                        self.runs.push(code);
                    }
                    let mut out = out.trim().to_string();
                    if out.is_empty() {
                        out =
                            "(ran without printing anything; print() what you need to see)".into();
                    }
                    let chars: Vec<char> = out.chars().collect();
                    if chars.len() > 3000 {
                        out = format!(
                            "{}\n...(truncated)...\n{}",
                            chars[..1500].iter().collect::<String>(),
                            chars[chars.len() - 1500..].iter().collect::<String>()
                        );
                    }
                    format!("status: {status}\n{out}")
                }
                other => {
                    format!("There is no tool named {other:?}. Available tools: {TOOL_NAMES}.")
                }
            })
        }
        .await;
        r.unwrap_or_else(|e| format!("tool failed: {e}")) // a busy kernel, a dead kernel, a timeout
    }

    /// One model turn on a blocking thread, streaming into `events`.
    async fn chat(
        server: &Server,
        msgs: Vec<Value>,
        tools: Option<Value>,
        max_tokens: Option<u32>,
        cancel: Arc<AtomicBool>,
        events: Option<mpsc::UnboundedSender<Event>>,
    ) -> Result<crate::llm::Reply> {
        let server = server.clone();
        tokio::task::spawn_blocking(move || {
            server.chat(&msgs, tools.as_ref(), max_tokens, &cancel, |p| {
                if let Some(tx) = &events {
                    let _ = tx.send(match p {
                        Piece::Token(t) => Event::Token(t.into()),
                        Piece::Thinking(t) => Event::Thinking(t.into()),
                    });
                }
            })
        })
        .await?
    }

    /// Answer `text`, calling tools as the model asks. Every run_code goes
    /// through `confirm`. On error or cancel the question is dropped from
    /// the history, so it can be asked again.
    pub async fn ask(
        &mut self,
        server: &Server,
        conn: Option<&Connection>,
        text: &str,
        events: mpsc::UnboundedSender<Event>,
        mut confirm: Confirm,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        self.runs.clear();
        self.history.push(json!({"role": "user", "content": text}));
        let r = self
            .answer(server, conn, &events, &mut confirm, &cancel)
            .await;
        match &r {
            Ok(answer) if !cancel.load(std::sync::atomic::Ordering::Relaxed) => {
                self.history
                    .push(json!({"role": "assistant", "content": answer}));
                let excess = self.history.len().saturating_sub(self.max_history);
                self.history.drain(..excess);
            }
            _ => {
                self.history.pop();
            }
        }
        r
    }

    async fn answer(
        &mut self,
        server: &Server,
        conn: Option<&Connection>,
        events: &mpsc::UnboundedSender<Event>,
        confirm: &mut Confirm,
        cancel: &Arc<AtomicBool>,
    ) -> Result<String> {
        let tools = conn.filter(|c| c.can_inspect()).map(|_| tools());
        let mut turn: Vec<Value> = Vec::new();
        for round in 0..=self.max_rounds {
            let last = round == self.max_rounds;
            let mut msgs = vec![self.system(conn)];
            msgs.extend(self.history.iter().cloned());
            msgs.extend(turn.iter().cloned());
            if last {
                // out of tool rounds: make it answer with what it has
                msgs.push(json!({"role": "user", "content": "Stop calling tools now and answer with what you have."}));
            }
            let offered = if last { None } else { tools.clone() };
            let r = Self::chat(
                server,
                msgs,
                offered,
                None,
                cancel.clone(),
                Some(events.clone()),
            )
            .await?;
            if cancel.load(std::sync::atomic::Ordering::Relaxed) || r.tool_calls.is_empty() || last
            {
                return Ok(r.content);
            }
            turn.push(
                json!({"role": "assistant", "content": r.content, "tool_calls": r.tool_calls}),
            );
            for call in &r.tool_calls {
                let f = &call["function"];
                let name = f["name"].as_str().unwrap_or("").to_string();
                let args = match &f["arguments"] {
                    Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                    Value::Null => json!({}),
                    v => v.clone(),
                };
                let arg = args["expr"].as_str().unwrap_or("").to_string();
                let _ = events.send(Event::Tool {
                    name: name.clone(),
                    arg,
                });
                let result = self.tool(conn, &name, &args, confirm).await;
                let _ = events.send(Event::ToolResult(result.clone()));
                turn.push(json!({"role": "tool", "tool_name": name, "content": result}));
            }
        }
        Ok(String::new())
    }

    /// One-line nudge after an error. No tools, no history, short.
    pub async fn hint(
        &self,
        server: &Server,
        conn: Option<&Connection>,
        cell: &Cell,
    ) -> Result<String> {
        let n = cell
            .count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        let msgs = vec![
            self.system(conn),
            json!({"role": "user", "content": format!("Cell In [{n}] just raised an error. In ONE sentence of at \
most 25 words, say the likely cause and the fix. No code block.")}),
        ];
        let r = Self::chat(
            server,
            msgs,
            None,
            Some(200),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await?;
        Ok(r.content.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    pub fn clear(&mut self) {
        self.history.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_blocks_found() {
        let t = "Here:\n```python\nx = 1\nprint(x)\n```\nand\n```r\nmean(a)\n```\n";
        assert_eq!(
            code_blocks(t),
            vec!["x = 1\nprint(x)".to_string(), "mean(a)".to_string()]
        );
    }

    #[test]
    fn system_prompt_without_session() {
        let a = Assistant::default();
        let s = a.system(None)["content"].as_str().unwrap().to_string();
        assert!(s.contains("live Python session") && s.contains("No session is attached yet"));
        assert!(!s.contains("{session}") && !s.contains("{lang_hints}"));
    }
}
