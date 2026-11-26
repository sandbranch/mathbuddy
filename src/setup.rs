//! `mathbuddy setup-r` and `mathbuddy doctor`.

use std::path::PathBuf;

use crate::{
    llm::{PREFERRED_MODELS, Server, find_model},
    rbridge::BRIDGE_R,
};

pub const RPROFILE_MARK: &str = "# mathbuddy bridge";

pub fn version_string() -> String {
    match option_env!("MATHBUDDY_COMMIT") {
        Some(c) => format!("mathbuddy {} ({c})", env!("CARGO_PKG_VERSION")),
        None => format!("mathbuddy {}", env!("CARGO_PKG_VERSION")),
    }
}

pub fn rprofile_path() -> PathBuf {
    std::env::var("R_PROFILE_USER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".Rprofile"))
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Where the bridge's R code lives on disk: ~/.local/share/mathbuddy/bridge.R.
pub fn bridge_file() -> PathBuf {
    let data = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/share"));
    data.join("mathbuddy").join("bridge.R")
}

/// Write the built-in bridge to disk if it isn't there or differs (a newer
/// mathbuddy), so R sessions started from now on load this version.
pub fn ensure_bridge_file() -> std::io::Result<PathBuf> {
    let path = bridge_file();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(BRIDGE_R) {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, BRIDGE_R)?;
    }
    Ok(path)
}

fn rprofile_line(bridge: &std::path::Path) -> String {
    format!(
        "if (interactive()) try(source(\"{}\"), silent = TRUE)  {RPROFILE_MARK}",
        bridge.display()
    )
}

/// Add the bridge to ~/.Rprofile (or $R_PROFILE_USER), once; an older
/// mathbuddy line that loads it from elsewhere is replaced.
pub fn setup_r(args: &[String]) -> i32 {
    let bridge = match ensure_bridge_file() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("could not write {}: {e}", bridge_file().display());
            return 1;
        }
    };
    let line = rprofile_line(&bridge);
    if matches!(args.first().map(String::as_str), Some("--print" | "-p")) {
        println!("{line}");
        return 0;
    }
    let path = rprofile_path();
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let new = if let Some(existing) = old.lines().find(|l| l.contains(RPROFILE_MARK)) {
        if existing == line {
            println!("already set up in {}", path.display());
            return 0;
        }
        println!(
            "replacing the old mathbuddy line in {}:\n  {existing}",
            path.display()
        );
        old.lines()
            .map(|l| {
                if l.contains(RPROFILE_MARK) {
                    line.as_str()
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    } else {
        let sep = if old.is_empty() || old.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!("{old}{sep}{line}\n")
    };
    if let Err(e) = std::fs::write(&path, new) {
        eprintln!("could not write {}: {e}", path.display());
        return 1;
    }
    println!(
        "added to {}:\n  {line}\nNew interactive R sessions (RStudio, terminal R) now load the bridge. For one that is\n\
         already running, paste this into its console:\n  source(\"{}\")\n\
         Note: a project's own .Rprofile replaces ~/.Rprofile; add the line there too if you use one.",
        path.display(),
        bridge.display()
    );
    0
}

/// Check this machine's setup and say how to fix what's missing.
pub async fn doctor() -> i32 {
    let mut problems = 0;
    let mut row = |state: &str, text: String, fix: &str| {
        if state == "fix" {
            problems += 1;
        }
        let fix = if fix.is_empty() {
            String::new()
        } else {
            format!("\n        -> {fix}")
        };
        println!("  {state:<5} {text}{fix}");
    };
    println!("{}", version_string());

    let want = std::env::var("MATHBUDDY_MODEL").ok();
    let llama_host = std::env::var("MATHBUDDY_LLAMACPP_HOST").ok();
    println!("Ollama (the model)");
    let ollama = Server::ollama(
        std::env::var("MATHBUDDY_OLLAMA_HOST").ok().as_deref(),
        false,
        None,
        false,
        8192,
    );
    let o = ollama.clone();
    let (ver, names) = tokio::task::spawn_blocking(move || (o.version(), o.models()))
        .await
        .unwrap();
    let names = match (ver, names) {
        (Ok(v), Ok(names)) => {
            row(
                "ok",
                format!("reachable at {} (Ollama {v})", ollama.base),
                "",
            );
            match &want {
                Some(w) if find_model(&names, w).is_some() => {
                    row("ok", format!("MATHBUDDY_MODEL={w}"), "")
                }
                Some(w) if llama_host.is_some() => row(
                    "--",
                    format!("MATHBUDDY_MODEL={w} is not on Ollama (checked under llama.cpp below)"),
                    "",
                ),
                Some(w) => row(
                    "fix",
                    format!("MATHBUDDY_MODEL={w} is not on this server"),
                    &format!("ollama pull {w}"),
                ),
                None => match PREFERRED_MODELS.iter().find_map(|m| find_model(&names, m)) {
                    Some(b) => row(
                        "ok",
                        format!("model: {b} (the best one this server has)"),
                        "",
                    ),
                    None => row(
                        "fix",
                        format!("none of {} is on this server", PREFERRED_MODELS.join(", ")),
                        "ollama pull qwen3.5   (or qwen3.5:4b for a 4 GB graphics card)",
                    ),
                },
            }
            names
        }
        (Err(e), _) | (_, Err(e)) => {
            row(
                "fix",
                format!("cannot reach Ollama at {} ({e})", ollama.base),
                "start it (systemctl --user start ollama), or point at another machine: mathbuddy --host ADDRESS:11434",
            );
            Vec::new()
        }
    };

    println!("llama.cpp (optional, more models)");
    match &llama_host {
        None => row(
            "--",
            "not set up (to use llama-server models: MATHBUDDY_LLAMACPP_HOST=ADDRESS:PORT)".into(),
            "",
        ),
        Some(h) => {
            let lc = Server::llamacpp(h, false);
            let l = lc.clone();
            let w = want.clone();
            let (cat, hit) = tokio::task::spawn_blocking(move || {
                let cat = l.catalog();
                let hit = w.as_deref().map(|w| l.find(w).ok().flatten());
                (cat, hit)
            })
            .await
            .unwrap();
            match cat {
                Ok(cat) => {
                    let loaded: Vec<&str> = cat
                        .iter()
                        .filter(|(_, on)| *on)
                        .map(|(n, _)| n.as_str())
                        .collect();
                    let lt = if loaded.is_empty() {
                        ", none loaded".into()
                    } else {
                        format!(", loaded: {}", loaded.join(", "))
                    };
                    row(
                        "ok",
                        format!("reachable at {}: {} model(s){lt}", lc.base, cat.len()),
                        "",
                    );
                    if let Some(w) = &want
                        && find_model(&names, w).is_none()
                    {
                        let found = hit.flatten().is_some();
                        row(
                            if found { "ok" } else { "fix" },
                            format!(
                                "MATHBUDDY_MODEL={w}{}",
                                if found { "" } else { " is on neither server" }
                            ),
                            if found {
                                ""
                            } else {
                                "pick one with /model in mathbuddy"
                            },
                        );
                    }
                }
                Err(e) => row(
                    "fix",
                    format!("cannot reach llama-server at {} ({e})", lc.base),
                    "start llama-server, or fix MATHBUDDY_LLAMACPP_HOST",
                ),
            }
        }
    }

    println!("R and RStudio (optional)");
    if which("Rscript").is_none() {
        row(
            "--",
            "R is not installed (only needed for R/RStudio sessions)".into(),
            "",
        );
    } else {
        let code = r#"for (p in c("httpuv","later","jsonlite","rstudioapi","ggplot2")) cat(p, requireNamespace(p, quietly = TRUE), "\n"); cat("R", R.version$major, R.version$minor, "\n")"#;
        let out = std::process::Command::new("Rscript")
            .args(["--no-init-file", "-e", code])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let have = |p: &str| {
            out.lines()
                .any(|l| l.split_whitespace().collect::<Vec<_>>() == [p, "TRUE"])
        };
        let rver = out
            .lines()
            .find(|l| l.starts_with("R "))
            .unwrap_or("R ?")
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(".");
        row("ok", format!("R {rver}"), "");
        let need: Vec<&str> = ["httpuv", "later", "jsonlite"]
            .into_iter()
            .filter(|p| !have(p))
            .collect();
        if need.is_empty() {
            row("ok", "bridge packages: httpuv, later, jsonlite".into(), "");
        } else {
            let list = need
                .iter()
                .map(|p| format!("\"{p}\""))
                .collect::<Vec<_>>()
                .join(", ");
            row(
                "fix",
                format!("bridge packages: missing {}", need.join(", ")),
                &format!("in R: install.packages(c({list}))"),
            );
        }
        for (p, why) in [
            ("rstudioapi", "/insert and /console in RStudio"),
            ("ggplot2", "nicer plots"),
        ] {
            if have(p) {
                row("ok", format!("{p} ({why})"), "");
            } else {
                row("--", format!("{p} ({why}): not installed"), "");
            }
        }
        let prof = rprofile_path();
        let text = std::fs::read_to_string(&prof).unwrap_or_default();
        match text.lines().find(|l| l.contains(RPROFILE_MARK)) {
            None => row(
                "fix",
                format!(
                    "{} does not load the bridge, so R sessions won't show up",
                    prof.display()
                ),
                "mathbuddy setup-r",
            ),
            Some(l) if l == rprofile_line(&bridge_file()) && bridge_file().exists() => {
                row("ok", format!("{} loads the bridge", prof.display()), "")
            }
            Some(_) => row(
                "fix",
                format!(
                    "{} loads a bridge from elsewhere (an older mathbuddy?)",
                    prof.display()
                ),
                "mathbuddy setup-r",
            ),
        }
    }

    println!("Other tools (optional)");
    match which("jotter") {
        Some(_) => row(
            "ok",
            "jotter (Jupyter notebooks in the terminal)".into(),
            "",
        ),
        None => row(
            "--",
            "jotter (Jupyter notebooks in the terminal): not installed".into(),
            "",
        ),
    }

    println!("Sessions I can see right now");
    let found = crate::session::rank(crate::session::discover(None, true).await);
    if found.is_empty() {
        println!("  (none: start jotter, or R/RStudio after setup-r)");
    }
    for (i, s) in found.iter().enumerate() {
        println!("  [{}] {}", i + 1, s.line());
    }
    if problems == 0 {
        println!("all good");
        0
    } else {
        println!(
            "{problems} thing{} to fix (see -> above)",
            if problems > 1 { "s" } else { "" }
        );
        1
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| PathBuf::from(d).join(name))
        .find(|p| p.is_file())
}
