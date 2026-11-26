//! Settings you write (`~/.config/mathbuddy/config.toml`) and choices
//! mathbuddy remembers between runs (`~/.local/state/mathbuddy/state.json`:
//! the last model and the last session you picked).
//!
//! Order of precedence: command-line options, then MATHBUDDY_* variables,
//! then the config file, then the built-in defaults.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What the config file may hold. Every field is optional.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Ollama's address (otherwise $OLLAMA_HOST, then 127.0.0.1:11434).
    pub ollama: Option<String>,
    /// llama-server addresses, single models or routers.
    pub llamacpp: Vec<String>,
    /// Model context window in tokens (Ollama).
    pub ctx: Option<u32>,
    /// Let thinking models think.
    pub think: Option<bool>,
    /// One-line hints when a cell raises an error.
    pub hints: Option<bool>,
    /// Keep the model off the GPU (Ollama).
    pub cpu: Option<bool>,
    /// CPU threads for the model (with cpu).
    pub threads: Option<u32>,
}

const TEMPLATE: &str = "\
# mathbuddy settings. Command-line options and MATHBUDDY_* variables win
# over these. Delete a line to go back to the default.

# Ollama's address (default: $OLLAMA_HOST, then 127.0.0.1:11434)
# ollama = \"127.0.0.1:11434\"

# llama.cpp llama-server addresses: single models or routers.
# /llamacpp ADDRESS in mathbuddy adds one here.
llamacpp = []

# ctx = 8192        # model context window in tokens (Ollama)
# think = false     # let thinking models think (slower, shown dimmed)
# hints = true      # one-line hints when a cell raises an error
# cpu = false       # keep the model off the graphics card (Ollama)
# threads = 8       # CPU threads for the model, with cpu = true
";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

pub fn config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
        .join("mathbuddy")
        .join("config.toml")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("mathbuddy")
}

fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

impl Config {
    /// The config file, or the defaults when there is none. An unreadable
    /// file gives the defaults and the reason.
    pub fn load() -> (Config, Option<String>) {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> (Config, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), None),
            Err(e) => return (Config::default(), Some(format!("{}: {e}", path.display()))),
        };
        match toml_edit::de::from_str::<Config>(&text) {
            Ok(c) => (c, None),
            Err(e) => (
                Config::default(),
                Some(format!(
                    "{} has a mistake, so it was ignored: {}",
                    path.display(),
                    e.to_string().trim()
                )),
            ),
        }
    }
}

/// Add a llama-server address to the config file, keeping whatever else is
/// in it (comments too). Creates the file from the template if needed.
/// Ok(false) if it was already there.
pub fn add_llamacpp(path: &Path, host: &str) -> Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_string(),
        Err(e) => return Err(e).context(path.display().to_string()),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} has a mistake", path.display()))?;
    let item = doc
        .entry("llamacpp")
        .or_insert(toml_edit::value(toml_edit::Array::new()));
    let arr = item
        .as_array_mut()
        .with_context(|| format!("llamacpp in {} isn't a list", path.display()))?;
    if arr.iter().any(|v| v.as_str() == Some(host)) {
        return Ok(false);
    }
    arr.push(host);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, doc.to_string())?;
    Ok(true)
}

/// The model you last chose, and where.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct LastModel {
    /// "ollama" or "llama.cpp"
    pub kind: String,
    /// The server's base URL.
    pub server: String,
    pub name: String,
}

/// The session you last chose: its connection file (or R registry file),
/// and its title, to find it again after its frontend restarts.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct LastSession {
    pub path: PathBuf,
    pub title: String,
    /// Its folder: the same title elsewhere (every Giac scratchpad is
    /// "Giac · jotter · scratchpad") is not the same session.
    #[serde(default)]
    pub cwd: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct State {
    pub model: Option<LastModel>,
    pub session: Option<LastSession>,
}

impl State {
    pub fn load() -> State {
        std::fs::read_to_string(state_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Change one thing and write it back (read fresh first, so two
    /// mathbuddys don't undo each other's other choice).
    pub fn update(f: impl FnOnce(&mut State)) {
        let mut s = State::load();
        f(&mut s);
        let dir = state_dir();
        let _ = std::fs::create_dir_all(&dir);
        let tmp = dir.join(format!("state.json.{}", std::process::id()));
        if let Ok(text) = serde_json::to_string_pretty(&s)
            && std::fs::write(&tmp, text).is_ok()
        {
            let _ = std::fs::rename(&tmp, state_path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_and_adding_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("mathbuddy-cfg-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.toml");
        assert_eq!(Config::load_from(&path), (Config::default(), None));
        assert!(add_llamacpp(&path, "127.0.0.1:8081").unwrap());
        assert!(!add_llamacpp(&path, "127.0.0.1:8081").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Ollama's address"), "{text}");
        let (c, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert_eq!(c.llamacpp, vec!["127.0.0.1:8081".to_string()]);

        std::fs::write(
            &path,
            "ollama = \"example:11434\"\nctx = 16384\nthinkk = true\n",
        )
        .unwrap();
        let (c, err) = Config::load_from(&path);
        assert_eq!(c, Config::default());
        assert!(err.unwrap().contains("thinkk"));
        std::fs::write(&path, "ollama = \"other:11434\"\nhints = false\n").unwrap();
        let (c, _) = Config::load_from(&path);
        assert_eq!(c.ollama.as_deref(), Some("other:11434"));
        assert_eq!(c.hints, Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
