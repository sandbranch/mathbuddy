//! The cells a jotter session already ran, read from the notebook jotter
//! has open. For kernels that can't tell (Giac before 0.2.2, Octave's
//! metakernel): jotter keeps an autosave of the notebook, outputs and
//! execution counts included, a few seconds behind the screen.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde_json::Value;

use crate::session::Cell;

/// Cells run in this kernel, oldest first, at most `max`: from jotter's
/// autosave of the notebook (or the notebook itself), if it was written
/// since the kernel started (`since`), so a leftover from an earlier run
/// doesn't count.
pub fn cells(pid: u32, cwd: &str, since: SystemTime, max: usize) -> Vec<Cell> {
    let Some(nb) = notebook(pid, cwd) else {
        return Vec::new();
    };
    let fresh = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t >= since)
    };
    // the autosave first (it has outputs even for a .py), then the file;
    // a .py notebook isn't JSON and is skipped
    [autosave_path(&nb), Some(nb)]
        .into_iter()
        .flatten()
        .filter(|p| fresh(p))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|doc| from_ipynb(&doc, max))
        .unwrap_or_default()
}

/// When jotter last wrote the notebook or its autosave: mathbuddy reads
/// the cells again when this changes (jotter autosaves every 30 s, so cells
/// from just before attaching can arrive a little later).
pub fn written(pid: u32, cwd: &str) -> Option<SystemTime> {
    let nb = notebook(pid, cwd)?;
    [autosave_path(&nb), Some(nb)]
        .into_iter()
        .flatten()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
}

/// The notebook a jotter process has open: its file argument, or the
/// scratchpad (`[scratch]` in its folder, as jotter names its autosave).
fn notebook(pid: u32, cwd: &str) -> Option<PathBuf> {
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = cmd
        .split(|&c| c == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    // options that take a value
    let mut skip = false;
    let mut file = None;
    for a in args.iter().skip(1) {
        if skip {
            skip = false;
            continue;
        }
        if a == "--kernel" || a == "--log" {
            skip = true;
            continue;
        }
        if !a.starts_with('-') {
            file = Some(a.clone());
        }
    }
    let dir = Path::new(cwd);
    Some(match file {
        Some(f) => dir.join(f),
        None => dir.join("[scratch]"),
    })
}

/// Where jotter autosaves `nb`: `<state>/jotter/autosave/<absolute path with
/// '/' as '%'>`, the name capped at its last 200 characters.
fn autosave_path(nb: &Path) -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    let name = nb.to_string_lossy().replace('%', "%%").replace('/', "%");
    let name = match name.char_indices().rev().nth(199) {
        Some((i, _)) => name[i..].to_string(),
        None => name,
    };
    Some(state.join("jotter/autosave").join(name))
}

/// The code cells that ran (an execution count), in the order they ran.
fn from_ipynb(doc: &Value, max: usize) -> Vec<Cell> {
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    };
    let mut cells: Vec<Cell> = doc["cells"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["cell_type"] == "code")
        .filter_map(|c| {
            let count = c["execution_count"].as_i64()?;
            let mut cell = Cell::new(format!("jotter-{count}"), Some(count), text(&c["source"]));
            for o in c["outputs"].as_array().into_iter().flatten() {
                match o["output_type"].as_str().unwrap_or("") {
                    "stream" => cell.outputs.push(text(&o["text"])),
                    "execute_result" | "display_data" => {
                        let plain = text(&o["data"]["text/plain"]);
                        if !plain.is_empty() {
                            cell.outputs.push(plain);
                        } else if o["data"]
                            .as_object()
                            .is_some_and(|d| d.keys().any(|k| k.starts_with("image/")))
                        {
                            cell.outputs.push("[plot]".into());
                        }
                    }
                    "error" => {
                        cell.error = Some(format!(
                            "{}: {}",
                            o["ename"].as_str().unwrap_or("Error"),
                            o["evalue"].as_str().unwrap_or("")
                        ))
                    }
                    _ => {}
                }
            }
            cell.done = true;
            Some(cell)
        })
        .filter(|c| !c.code.trim().is_empty())
        .collect();
    cells.sort_by_key(|c| c.count);
    let start = cells.len().saturating_sub(max);
    cells.split_off(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ran_cells_in_order_with_outputs() {
        let doc = json!({"cells": [
            {"cell_type": "markdown", "source": "# notes"},
            {"cell_type": "code", "execution_count": 3, "source": ["solve(x^2+2*x+1=0, x)"],
             "outputs": [{"output_type": "execute_result", "data": {"text/plain": "list[-1]"}}]},
            {"cell_type": "code", "execution_count": null, "source": "not run yet"},
            {"cell_type": "code", "execution_count": 1, "source": "A:=3",
             "outputs": [{"output_type": "stream", "text": ["hello\n"]}]},
            {"cell_type": "code", "execution_count": 2, "source": "plot(x)",
             "outputs": [{"output_type": "display_data", "data": {"image/svg+xml": "<svg/>"}}]},
            {"cell_type": "code", "execution_count": 4, "source": "1/0",
             "outputs": [{"output_type": "error", "ename": "ZeroDivisionError", "evalue": "boom"}]}
        ]});
        let cells = from_ipynb(&doc, 10);
        let codes: Vec<&str> = cells.iter().map(|c| c.code.as_str()).collect();
        assert_eq!(codes, ["A:=3", "plot(x)", "solve(x^2+2*x+1=0, x)", "1/0"]);
        assert_eq!(cells[0].outputs, ["hello\n"]);
        assert_eq!(cells[1].outputs, ["[plot]"]);
        assert_eq!(cells[2].outputs, ["list[-1]"]);
        assert_eq!(cells[3].error.as_deref(), Some("ZeroDivisionError: boom"));
        assert_eq!(from_ipynb(&doc, 2).len(), 2);
    }

    #[test]
    fn autosave_names_match_jotters() {
        // SAFETY: test-only, single assignment before reading it
        unsafe { std::env::set_var("XDG_STATE_HOME", "/st") };
        assert_eq!(
            autosave_path(Path::new("/home/me/[scratch]")).unwrap(),
            PathBuf::from("/st/jotter/autosave/%home%me%[scratch]")
        );
        assert_eq!(
            autosave_path(Path::new("/a/100%/n.ipynb")).unwrap(),
            PathBuf::from("/st/jotter/autosave/%a%100%%%n.ipynb")
        );
    }
}
