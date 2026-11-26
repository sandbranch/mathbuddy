//! mathbuddy: a small local LLM agent that attaches to your live math
//! session (jotter or any Jupyter kernel, R and RStudio), looks at your
//! variables and runs code there when you approve it.

use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use clap::Parser;

use mathbuddy::{
    assistant::{Confirm, Event},
    core::{Core, Settings},
    setup,
};

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// A small local LLM agent for your live math session: jotter or any
/// Jupyter kernel, R and RStudio.
#[derive(Parser, Debug)]
#[command(name = "mathbuddy", version = setup::version_string().trim_start_matches("mathbuddy ").to_string(),
          after_help = "other commands:\n  mathbuddy doctor     check this machine's setup, say how to fix what's missing\n  \
                        mathbuddy setup-r    make R and RStudio sessions visible to mathbuddy (~/.Rprofile)")]
struct Args {
    /// Ask once and exit (otherwise interactive)
    question: Vec<String>,
    /// Model, on Ollama or on llama.cpp with --llamacpp (default: the best Ollama has; also MATHBUDDY_MODEL)
    #[arg(short, long, env = "MATHBUDDY_MODEL")]
    model: Option<String>,
    /// Ollama address (default $OLLAMA_HOST; also MATHBUDDY_OLLAMA_HOST)
    #[arg(long, env = "MATHBUDDY_OLLAMA_HOST")]
    host: Option<String>,
    /// llama.cpp llama-server address, one model or a router (e.g. 127.0.0.1:8081)
    #[arg(long, value_name = "HOST", env = "MATHBUDDY_LLAMACPP_HOST")]
    llamacpp: Option<String>,
    /// Keep the model off the GPU (Ollama: num_gpu=0)
    #[arg(long, env = "MATHBUDDY_CPU", value_parser = clap::builder::FalseyValueParser::new())]
    cpu: bool,
    /// CPU threads for the model (with --cpu)
    #[arg(long, env = "MATHBUDDY_THREADS")]
    threads: Option<u32>,
    /// Model context window in tokens (Ollama)
    #[arg(long, default_value_t = 8192, env = "MATHBUDDY_CTX")]
    ctx: u32,
    /// Let thinking models think (slower, shown dimmed)
    #[arg(long)]
    think: bool,
    /// Attach to this session: a number from --list, or words to match (rstudio, jotter, a notebook)
    #[arg(short, long)]
    session: Option<String>,
    /// List the sessions I can attach to, and exit
    #[arg(short, long)]
    list: bool,
    /// Only Jupyter connection files matching this pattern (e.g. 'kernel-*.json')
    #[arg(long)]
    kernel: Option<String>,
    /// Attach to this exact Jupyter connection file
    #[arg(short = 'f', long)]
    connection_file: Option<PathBuf>,
    /// No automatic hints on errors
    #[arg(long)]
    no_hints: bool,
    /// With a one-shot question: also run the last code block
    #[arg(long)]
    run: bool,
    /// Let the model run code in the session without asking each time
    #[arg(short, long)]
    yes: bool,
}

impl Args {
    fn settings(&self) -> Settings {
        Settings {
            model: self.model.clone(),
            host: self.host.clone(),
            llamacpp: self.llamacpp.clone(),
            cpu: self.cpu,
            threads: self.threads,
            ctx: self.ctx,
            think: self.think,
            session: self.session.clone(),
            kernel: self.kernel.clone(),
            connection_file: self.connection_file.clone(),
            hints: !self.no_hints,
            yes: self.yes,
        }
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match argv.get(1).map(String::as_str) {
        Some("setup-r") => std::process::exit(setup::setup_r(&argv[2..])),
        Some("doctor") => std::process::exit(setup::doctor().await),
        _ => {}
    }
    let args = Args::parse();
    // R sessions started from now on load this version's bridge
    let _ = setup::ensure_bridge_file();
    let (mut core, done_rx) = Core::new(args.settings());
    if args.list {
        let found = core.find_sessions(true).await;
        if found.is_empty() {
            println!(
                "(no sessions found: start jotter, or R/RStudio with the bridge loaded; `mathbuddy setup-r` sets that up)"
            );
        }
        for l in core.session_lines(&found) {
            println!("{l}");
        }
        return;
    }
    if !args.question.is_empty() {
        std::process::exit(once(&mut core, &args.question.join(" "), args.run, args.think).await);
    }
    if let Err(e) = mathbuddy::ui::run(core, done_rx, args.think).await {
        eprintln!("mathbuddy: {e}");
        std::process::exit(1);
    }
}

fn flush_notes(core: &mut Core) {
    for n in core.notes.drain(..) {
        println!("{n}");
    }
}

/// Ask one question on the command line, print the answer, exit.
async fn once(core: &mut Core, question: &str, run_last: bool, show_thinking: bool) -> i32 {
    println!("{}", setup::version_string());
    core.choose_model().await;
    let target = core.settings.session.clone();
    core.attach(target.as_deref(), false).await;
    flush_notes(core);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let yes = core.always_run;
    let server = core.models.server().clone();
    let mut printer = Printer {
        show_thinking,
        ..Default::default()
    };
    let result = {
        let confirm: Confirm = Box::new(move |code: String| {
            Box::pin(async move {
                println!("\n{BOLD}mathbuddy wants to run this in your session:{RESET}");
                for l in code.lines() {
                    println!("  │ {l}");
                }
                if yes {
                    println!("{DIM}(approved: --yes){RESET}");
                } else {
                    println!(
                        "{DIM}(declined: pass --yes to allow runs from the command line){RESET}"
                    );
                }
                yes
            })
        });
        let ask = core
            .assistant
            .ask(&server, core.conn.as_deref(), question, tx, confirm, cancel);
        tokio::pin!(ask);
        loop {
            tokio::select! {
                biased;
                Some(ev) = rx.recv() => printer.event(ev),
                r = &mut ask => {
                    while let Ok(ev) = rx.try_recv() {
                        printer.event(ev);
                    }
                    break r;
                }
            }
        }
    };
    printer.end();
    match result {
        Ok(answer) => {
            if let Some(hint) = core.take_blocks(&answer) {
                println!("{DIM}{hint}{RESET}");
            }
            if run_last && let Some(code) = core.blocks.last().cloned() {
                eprintln!("--- /run");
                match &core.conn {
                    Some(c) => match c.run(&code).await {
                        Ok((status, text)) => {
                            let t = text.trim_end();
                            println!(
                                "{}",
                                if t.is_empty() {
                                    format!("(done, status {status}, no output)")
                                } else {
                                    t.into()
                                }
                            );
                        }
                        Err(e) => println!("(run failed: {e})"),
                    },
                    None => println!("(no session attached yet)"),
                }
            }
            0
        }
        Err(e) => {
            println!("(error: {e})");
            1
        }
    }
}

/// Prints the answer as it streams: text as is, thinking dimmed (or one
/// "(thinking...)" line), tool calls and their results dimmed.
#[derive(Default)]
struct Printer {
    show_thinking: bool,
    thinking: bool,
    said_thinking: bool,
    col0: bool,
}

impl Printer {
    fn write(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        print!("{s}");
        let _ = std::io::stdout().flush();
        self.col0 = s.ends_with('\n');
    }

    fn newline(&mut self) {
        if !self.col0 {
            self.write("\n");
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Token(t) => {
                if self.thinking {
                    self.write(&format!("{RESET}\n"));
                    self.thinking = false;
                }
                self.write(&t);
            }
            Event::Thinking(t) if self.show_thinking => {
                if !self.thinking {
                    self.write(DIM);
                    self.thinking = true;
                }
                self.write(&t);
            }
            Event::Thinking(_) => {
                if !self.said_thinking {
                    self.said_thinking = true;
                    self.write(&format!("{DIM}(thinking...){RESET}\n"));
                }
            }
            Event::Tool { name, arg } => {
                if name == "run_code" {
                    return; // the approval shows the code
                }
                self.newline();
                let a = if arg.is_empty() {
                    String::new()
                } else {
                    format!("({arg})")
                };
                self.write(&format!("{DIM}[{name}{a}]{RESET}\n"));
            }
            Event::ToolResult(text) => {
                if !text.starts_with("status:") {
                    return;
                }
                // run_code output: show the real numbers
                self.newline();
                let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                let shown: Vec<&str> = if lines.first() == Some(&"status: error") && lines.len() > 8
                {
                    // a traceback's point is its end
                    let mut v = vec![lines[0], "  ..."];
                    v.extend(&lines[lines.len() - 6..]);
                    v
                } else {
                    let mut v: Vec<&str> = lines.iter().take(14).copied().collect();
                    if lines.len() > 14 {
                        v.push("  ...");
                    }
                    v
                };
                let body: Vec<String> = shown.iter().map(|l| format!("  → {l}")).collect();
                self.write(&format!("{DIM}{}{RESET}\n", body.join("\n")));
            }
        }
    }

    fn end(&mut self) {
        self.write(&format!("{RESET}\n"));
    }
}
