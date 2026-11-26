//! mathbuddy's state and commands, shared by the full-screen interface and
//! the one-shot command line. Everything it has to say goes into `notes`,
//! which the front end shows however it shows things.

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use tokio::sync::mpsc;

use crate::{
    assistant::Assistant,
    config::{LastModel, LastSession, State},
    llm::{Models, PREFERRED_MODELS, Server, ServerKind, find_model},
    session::{CellDone, Connection, Found, discover, pick, rank},
};

/// Command-line settings the core needs.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub model: Option<String>,
    pub host: Option<String>,
    pub llamacpp: Vec<String>,
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
    /// You chose the attached session yourself (-s, /attach, the picker):
    /// mathbuddy doesn't move to a new session next to you.
    pub chosen: bool,
    /// When jotter last wrote the attached notebook, as read.
    jotter_written: Option<std::time::SystemTime>,
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
        for h in &s.llamacpp {
            servers.push(Server::llamacpp(h, s.think));
        }
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
            chosen: false,
            jotter_written: None,
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

    /// -m picks a model on any server. Without it, the one you chose last
    /// time, if its server still has it. Otherwise the best one Ollama has,
    /// or else a model a llama.cpp server already has loaded: never one it
    /// would have to load, since that may unload another in use.
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
                    self.remember_model();
                }
                None => self.say(format!(
                    "(model {want} isn't on any of your servers; /model lists what they have)"
                )),
            }
            return;
        }
        if let Some(last) = State::load().model {
            match self.restore_model(&last).await {
                Ok(()) => return,
                Err(why) => self.say(format!(
                    "(last time's model, {} on {}, {why})",
                    last.name, last.server
                )),
            }
        }
        let ollama = self.models.servers[0].clone();
        let names = match tokio::task::spawn_blocking(move || ollama.models())
            .await
            .unwrap()
        {
            Ok(n) => n,
            Err(e) => {
                self.say(format!(
                    "(can't reach Ollama at {}: {e})",
                    self.models.servers[0].base
                ));
                self.use_loaded_llamacpp().await;
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

    /// Use the remembered model if its server is configured and has it.
    async fn restore_model(&mut self, last: &LastModel) -> Result<(), String> {
        let Some(i) = self
            .models
            .servers
            .iter()
            .position(|s| s.base == last.server && s.kind.name() == last.kind)
        else {
            return Err("isn't on a server you have set up now".into());
        };
        let s = self.models.servers[i].clone();
        let cat = tokio::task::spawn_blocking(move || s.catalog())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("can't be reached ({e})"))?;
        if !cat.iter().any(|(n, _)| *n == last.name) {
            return Err("is no longer there".into());
        }
        // never load it in place of a model something else is using; F3 asks
        if self.models.servers[i].kind == ServerKind::LlamaCpp
            && self.llamacpp_swap_warning(i, &last.name).await.is_some()
        {
            return Err(
                "isn't loaded now, and loading it could unload another model \
                        (F3 loads it when you want)"
                    .into(),
            );
        }
        self.models.use_model(i, &last.name);
        let label = self.models.label();
        self.say(format!("(model: {label}, as last time; F3 picks another)"));
        Ok(())
    }

    /// Without Ollama: a model some llama.cpp server already has loaded.
    async fn use_loaded_llamacpp(&mut self) {
        for (i, cat) in self.catalog().await {
            if self.models.servers[i].kind != ServerKind::LlamaCpp {
                continue;
            }
            match cat {
                Ok(cat) => {
                    if let Some((name, _)) = cat.iter().find(|(_, loaded)| *loaded) {
                        self.models.use_model(i, name);
                        let label = self.models.label();
                        self.say(format!(
                            "(model: {label}, already loaded there; F3 picks another)"
                        ));
                        return;
                    }
                }
                Err(e) => self.say(format!(
                    "(can't reach llama-server at {}: {e})",
                    self.models.servers[i].base
                )),
            }
        }
        if self.models.servers.len() > 1 {
            self.say("(no model chosen yet: F3 lists what your servers have)");
        } else {
            self.say(
                "(no model to use: start Ollama, or add a llama-server with /llamacpp ADDRESS)",
            );
        }
    }

    fn remember_model(&self) {
        let s = self.models.server();
        let last = LastModel {
            kind: s.kind.name().into(),
            server: s.base.clone(),
            name: s.model.clone(),
        };
        State::update(|st| st.model = Some(last));
    }

    /// Remember the attached session as your choice for next time (and
    /// for now: no moving to new sessions on its own).
    pub fn remember_session(&mut self) {
        self.chosen = true;
        if let Some(s) = &self.current {
            let last = LastSession {
                path: s.path.clone(),
                title: s.title(),
                cwd: s.cwd.clone(),
            };
            State::update(|st| st.session = Some(last));
        }
    }

    /// Add a llama-server (and save it in the config file).
    pub async fn add_llamacpp(&mut self, host: &str) {
        let server = Server::llamacpp(host, self.settings.think);
        if self.models.servers.iter().any(|s| s.base == server.base) {
            self.say(format!("({} is already one of your servers)", server.base));
            return;
        }
        let probe = server.clone();
        match tokio::task::spawn_blocking(move || probe.catalog()).await {
            Ok(Ok(cat)) => self.say(format!(
                "(llama-server at {}: {} model(s); F3 lists them)",
                server.base,
                cat.len()
            )),
            _ => self.say(format!(
                "(nothing answers at {} yet; added anyway, F3 shows it when it does)",
                server.base
            )),
        }
        let path = crate::config::config_path();
        match crate::config::add_llamacpp(&path, host) {
            Ok(_) => self.say(format!("(saved in {})", path.display())),
            Err(e) => self.say(format!("(couldn't save it in the config: {e:#})")),
        }
        self.settings.llamacpp.push(host.to_string());
        self.models.servers.push(server);
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
        self.remember_model();
    }

    // -- sessions ----------------------------------------------------------

    pub async fn find_sessions(&self, probe_r: bool) -> Vec<Found> {
        let pats = self.settings.kernel.as_ref().map(|k| vec![k.clone()]);
        let last = State::load().session;
        rank(discover(pats.as_deref(), probe_r).await, last.as_ref())
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
                // the conversation was about the other session: a small
                // model mixes them up, so it starts again (the screen keeps it)
                if self.conn.is_some() && !self.assistant.history.is_empty() {
                    self.assistant.clear();
                    self.say("(the model starts a new conversation for this session)");
                }
                self.conn = Some(Arc::new(c));
                self.current = Some(s.clone());
                self.jotter_written = None;
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
        // jotter's notebook changed: cells that ran before we attached
        // may be in it now
        if let (Some(c), Some(s)) = (&self.conn, &self.current)
            && let Some(pid) = s.pid.filter(|_| s.frontend == "jotter")
        {
            let written = crate::jotter::written(pid, &s.cwd);
            if written.is_some() && written != self.jotter_written {
                self.jotter_written = written;
                s.seed_from_jotter(c);
            }
        }
        let found = self.find_sessions(false).await;
        let fresh: Vec<Found> = found
            .iter()
            .filter(|s| !self.known.contains(&s.path))
            .cloned()
            .collect();
        if fresh.is_empty() {
            return;
        }
        self.known.extend(found.iter().map(|s| s.path.clone()));
        self.listing = found.clone();
        // a session next to you (or the one you run inside) replaces one
        // that isn't, unless you chose the current one yourself
        let here = |score: u8| score >= 3;
        let current_score = self
            .current
            .as_ref()
            .and_then(|c| found.iter().find(|s| s.path == c.path))
            .map(|s| s.score)
            .unwrap_or(0);
        if !self.chosen
            && !here(current_score)
            && let Some(best) = fresh.iter().filter(|s| here(s.score)).max_by(|a, b| {
                (a.score, a.started)
                    .partial_cmp(&(b.score, b.started))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            let best = best.clone();
            if self.attach_to(&best, true).await {
                self.say("(switched: /sessions to go back)");
                return;
            }
        }
        // the others are only mentioned when they are yours: next to you,
        // in your folder, or your choice last time; all are in /sessions
        for s in fresh.iter().filter(|s| s.score > 0) {
            self.say(format!(
                "(new session: {}, {}; /sessions to switch)",
                s.title(),
                s.reason
            ));
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
