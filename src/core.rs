//! mathbuddy's state and commands, shared by the full-screen interface and
//! the one-shot command line. Everything it has to say goes into `notes`,
//! which the front end shows however it shows things.

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use tokio::sync::mpsc;

use crate::{
    assistant::Assistant,
    llm::{Models, PREFERRED_MODELS, Server, ServerKind, find_model},
    session::{CellDone, Connection, Found, discover, pick, rank},
};

/// Command-line settings the core needs.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub model: Option<String>,
    pub host: Option<String>,
    pub llamacpp: Option<String>,
    pub cpu: bool,
    pub threads: Option<u32>,
    pub ctx: u32,
    pub think: bool,
    pub session: Option<String>,
    pub kernel: Option<String>,
    pub connection_file: Option<PathBuf>,
    pub hints: bool,
    pub yes: bool,
}

pub struct Core {
    pub settings: Settings,
    pub models: Models,
    pub assistant: Assistant,
    /// Shared, so an answer can use it from its own task while the screen draws.
    pub conn: Option<Arc<Connection>>,
    /// The session attached to, as listed (None with -f).
    pub current: Option<Found>,
    /// The last list of sessions shown, for /attach N.
    pub listing: Vec<Found>,
    pub known: HashSet<PathBuf>,
    /// Code blocks of the last answer (code that ran first), for /run, /copy...
    pub blocks: Vec<String>,
    pub always_run: bool,
    pub hints: bool,
    pub notes: Vec<String>,
    pub done_tx: mpsc::UnboundedSender<CellDone>,
}

impl Core {
    pub fn new(s: Settings) -> (Self, mpsc::UnboundedReceiver<CellDone>) {
        let mut servers = vec![Server::ollama(
            s.host.as_deref(),
            s.cpu,
            s.threads,
            s.think,
            s.ctx,
        )];
        if let Some(h) = &s.llamacpp {
            servers.push(Server::llamacpp(h, s.think));
        }
        servers[0].model = PREFERRED_MODELS[0].into();
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let core = Core {
            always_run: s.yes,
            hints: s.hints,
            settings: s,
            models: Models {
                servers,
                current: 0,
            },
            assistant: Assistant::default(),
            conn: None,
            current: None,
            listing: Vec::new(),
            known: HashSet::new(),
            blocks: Vec::new(),
            notes: Vec::new(),
            done_tx,
        };
        (core, done_rx)
    }

    pub fn say(&mut self, s: impl Into<String>) {
        self.notes.push(s.into());
    }

    // -- models ----------------------------------------------------------

    /// -m picks a model on any server. Without it, the best one Ollama has:
    /// never a llama.cpp model, since asking a llama.cpp router for a model
    /// it hasn't loaded makes it load one, maybe in place of another.
    pub async fn choose_model(&mut self) {
        if let Some(want) = self.settings.model.clone() {
            match self.locate(&want).await {
                Some((i, name)) => {
                    if self.models.servers[i].kind == ServerKind::LlamaCpp
                        && let Some(warning) = self.llamacpp_swap_warning(i, &name).await
                    {
                        self.say(warning); // named with -m: warn, don't ask
                    }
                    self.models.use_model(i, &name);
                }
                None => self.say(format!(
                    "(model {want} isn't on any of your servers; /model lists what they have)"
                )),
            }
            return;
        }
        let ollama = self.models.servers[0].clone();
        let names = match tokio::task::spawn_blocking(move || ollama.models())
            .await
            .unwrap()
        {
            Ok(n) => n,
            Err(e) => {
                let more = if self.models.servers.len() > 1 {
                    "; /model lists the llama.cpp models"
                } else {
                    ""
                };
                self.say(format!(
                    "(can't reach Ollama at {}: {e}{more})",
                    self.models.servers[0].base
                ));
                return;
            }
        };
        match PREFERRED_MODELS.iter().find_map(|m| find_model(&names, m)) {
            Some(found) => {
                self.models.use_model(0, &found);
                self.say(format!(
                    "(model: {found}, the best one this Ollama has; -m picks another)"
                ));
            }
            None => self.say(format!(
                "(none of {} is on {}; it has: {}. Pick one with -m, or `ollama pull qwen3.5`)",
                PREFERRED_MODELS.join(", "),
                self.models.servers[0].base,
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            )),
        }
    }

    pub async fn locate(&self, want: &str) -> Option<(usize, String)> {
        let servers = self.models.servers.clone();
        let want = want.to_string();
        tokio::task::spawn_blocking(move || {
            Models {
                servers,
                current: 0,
            }
            .locate(&want)
        })
        .await
        .ok()
        .flatten()
    }

    /// Every server's models: (server index, Ok([(name, loaded)]) or the error).
    pub async fn catalog(&self) -> Vec<(usize, Result<Vec<(String, bool)>, String>)> {
        let servers = self.models.servers.clone();
        tokio::task::spawn_blocking(move || {
            servers
                .iter()
                .enumerate()
                .map(|(i, s)| (i, s.catalog().map_err(|e| e.to_string())))
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// A llama.cpp router loads a model on first use and may unload another
    /// to make room, cutting into whatever uses it. Some(warning) if so.
    pub async fn llamacpp_swap_warning(&self, server: usize, name: &str) -> Option<String> {
        let s = self.models.servers[server].clone();
        let cat = tokio::task::spawn_blocking(move || s.catalog())
            .await
            .ok()?
            .ok()?;
        if cat.iter().any(|(n, loaded)| n == name && *loaded) {
            return None;
        }
        let others: Vec<&str> = cat
            .iter()
            .filter(|(n, l)| *l && n != name)
            .map(|(n, _)| n.as_str())
            .collect();
        if others.is_empty() {
            return None;
        }
        Some(format!(
            "({name} isn't loaded on {}. Asking it something loads it, and the server may unload {} to make room, \
             interrupting anything using it.)",
            self.models.servers[server].base,
            others.join(", ")
        ))
    }

    pub fn switch_model(&mut self, server: usize, name: &str) {
        self.models.use_model(server, name);
        let note = if self.models.servers[server].kind == ServerKind::LlamaCpp
            && (self.settings.cpu || self.settings.threads.is_some())
        {
            "; --cpu and --threads apply to Ollama only, llama.cpp runs as its preset says"
        } else {
            ""
        };
        let label = self.models.label();
        self.say(format!("(model: {label}{note})"));
    }

    // -- sessions ----------------------------------------------------------

    pub async fn find_sessions(&self, probe_r: bool) -> Vec<Found> {
        let pats = self.settings.kernel.as_ref().map(|k| vec![k.clone()]);
        rank(discover(pats.as_deref(), probe_r).await)
    }

    pub fn session_lines(&self, found: &[Found]) -> Vec<String> {
        let cur = self.conn.as_ref().map(|c| c.path().to_path_buf());
        found
            .iter()
            .enumerate()
            .map(|(i, s)| {
                format!(
                    " {}[{}] {}",
                    if Some(&s.path) == cur.as_ref() {
                        "*"
                    } else {
                        " "
                    },
                    i + 1,
                    s.line()
                )
            })
            .collect()
    }

    pub async fn attach(&mut self, target: Option<&str>, quiet: bool) -> bool {
        if let Some(cf) = self.settings.connection_file.clone() {
            if self.conn.is_none()
                && crate::kernel::kernel_alive(&cf, std::time::Duration::from_secs(2)).await
            {
                match crate::kernel::KernelWatch::connect(&cf, self.done_tx.clone()).await {
                    Ok(k) => {
                        let name = cf
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        self.say(format!("attached: {} kernel, {name}", k.kernel_name));
                        self.conn = Some(Arc::new(Connection::Kernel(k)));
                        self.current = None;
                    }
                    Err(e) => self.say(format!("(could not attach to {}: {e})", cf.display())),
                }
            }
            return self.conn.is_some();
        }
        let found = self.find_sessions(true).await;
        self.listing = found.clone();
        self.known = found.iter().map(|s| s.path.clone()).collect();
        let Some(s) = pick(&found, target).cloned() else {
            if !quiet {
                match target {
                    Some(t) => self.say(format!("(no session matches {t:?}; /sessions lists them)")),
                    None => self.say(
                        "(no sessions yet: start jotter, or R/RStudio with the bridge loaded; I'll attach when one appears)",
                    ),
                }
            }
            return false;
        };
        if !self.attach_to(&s, quiet).await {
            return false;
        }
        if found.len() > 1 && target.is_none() {
            self.say("sessions running (/sessions to switch):");
            for l in self.session_lines(&found) {
                self.say(l);
            }
        }
        true
    }

    pub async fn attach_to(&mut self, s: &Found, quiet: bool) -> bool {
        if self.conn.as_ref().is_some_and(|c| c.path() == s.path) {
            if !quiet {
                self.say(format!("(already attached to {})", s.title()));
            }
            return true;
        }
        match s.connect(self.done_tx.clone()).await {
            Ok(c) => {
                let seen = c.recent(100).len();
                let why = if s.reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", s.reason)
                };
                let earlier = if seen > 0 {
                    format!(", {seen} earlier commands seen")
                } else {
                    String::new()
                };
                self.say(format!("attached: {}{why}{earlier}", s.title()));
                self.conn = Some(Arc::new(c));
                self.current = Some(s.clone());
                true
            }
            Err(e) => {
                self.say(format!("(could not attach to {}: {e})", s.title()));
                false
            }
        }
    }

    /// Every couple of seconds: attach when a session appears, notice new
    /// ones, drop the attached one when it goes away.
    pub async fn tick(&mut self) {
        if self.settings.connection_file.is_some() {
            if self.conn.is_none() {
                self.attach(None, true).await;
            }
            return;
        }
        if let Some(c) = &self.conn
            && !c.alive().await
        {
            let old = self.current.as_ref().map(|s| s.title()).unwrap_or_else(|| {
                c.path()
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            self.conn = None;
            self.current = None;
            self.say(format!(
                "(session {old} went away; waiting for the next one, /sessions to pick)"
            ));
        }
        if self.conn.is_none() {
            let target = self.settings.session.clone();
            self.attach(target.as_deref(), true).await;
            return;
        }
        let found = self.find_sessions(false).await;
        let fresh: Vec<Found> = found
            .iter()
            .filter(|s| !self.known.contains(&s.path))
            .cloned()
            .collect();
        if !fresh.is_empty() {
            self.known.extend(found.iter().map(|s| s.path.clone()));
            self.listing = found.clone();
            for s in fresh {
                let why = if s.score > 0 {
                    format!(", {}", s.reason)
                } else {
                    String::new()
                };
                self.say(format!(
                    "(new session: {}{why}; /sessions to switch)",
                    s.title()
                ));
            }
        }
    }

    /// Status-line text for the session.
    pub fn session_label(&self) -> String {
        match (&self.conn, &self.current) {
            (Some(c), Some(s)) => format!("{} · {} seen", s.title(), c.recent(100).len()),
            (Some(c), None) => format!("{} kernel · {} seen", c.language(), c.recent(100).len()),
            (None, _) => "no session (/sessions)".into(),
        }
    }

    /// Code block `arg` (1-based, default the last) of the last answer.
    pub fn pick_block(&mut self, arg: &str) -> Option<String> {
        if self.blocks.is_empty() {
            self.say("(no code in the last answer)");
            return None;
        }
        let n = if arg.is_empty() {
            Some(self.blocks.len())
        } else {
            arg.parse::<usize>().ok()
        };
        match n
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| self.blocks.get(i))
        {
            Some(b) => Some(b.clone()),
            None => {
                let n = self.blocks.len();
                self.say(format!("(code blocks are 1 to {n})"));
                None
            }
        }
    }

    /// The code blocks of an answer: code that ran first (so the model
    /// needn't repeat it), then the answer's own blocks.
    pub fn take_blocks(&mut self, answer: &str) -> Option<String> {
        let runs = self.assistant.runs.clone();
        let mut blocks = runs.clone();
        for b in crate::assistant::code_blocks(answer) {
            if !runs.iter().any(|r| r.trim() == b.trim()) {
                blocks.push(b);
            }
        }
        self.blocks = blocks;
        let n = self.blocks.len();
        if n == 0 {
            return None;
        }
        let only_ran = !runs.is_empty() && n == runs.len();
        let rstudio = self.conn.as_ref().is_some_and(|c| c.rstudio_api());
        let mut cmds: Vec<&str> = Vec::new();
        if rstudio {
            cmds.push("/insert");
        }
        cmds.extend(["/copy", "/show"]);
        if !only_ran {
            cmds.push("/run");
        }
        let what = if only_ran {
            "code that ran".to_string()
        } else {
            format!("{n} code block{}", if n > 1 { "s" } else { "" })
        };
        Some(format!(
            "({what}: {}{})",
            cmds.join(", "),
            if n > 1 { " with a number" } else { "" }
        ))
    }
}
