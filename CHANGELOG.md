# Changelog

## 0.3.0 (2026-11-26)

Upgrading: mathbuddy is now one Rust program. Install it with
`cargo install --git https://github.com/sandbranch/mathbuddy`, run
`mathbuddy setup-r` once (it points `~/.Rprofile` at the new bridge file), and
remove the old Python install (`uv tool uninstall mathbuddy`).

- **One program, written in Rust.** No Python environment to install or keep
  working; the R bridge is built in. It connects to kernels the way jotter
  does. Same prompt, tools and approval rule as before.
- **Full-screen.** The conversation, your question box, a bar with buttons
  (F1 help, F2 variables, F3 model, F4 session), a variables panel that
  stays up to date, pop-ups for picking and for approving runs. Mouse and
  keyboard. While it answers, you can scroll, and Ctrl-C stops it.
- **Giac.** mathbuddy looks at Giac variables too (giac-kernel evaluates
  `user_expressions` since 0.2.1), each with its type and a short value,
  and knows Giac's syntax.
- **It follows the session next to you.** When a session starts in the same
  tmux window (you open jotter beside mathbuddy), mathbuddy moves to it,
  unless you picked the current one yourself. Other new sessions are only
  mentioned when they are yours (same folder, your last choice); all are in
  `/sessions` (or `/session`). After a switch the model starts a new
  conversation, so it doesn't mix the two sessions up.
- **It sees what ran before it attached**, in jotter with any kernel: from
  the kernel's history where it keeps one (Python, Giac from 0.2.2), else
  from jotter's notebook and its autosave (Octave, older Giac), which jotter
  writes every 30 seconds.
- **Copy text with the mouse.** Drag over the conversation: it is highlighted and
  copied when you let go (Shift-drag still gives the terminal's own selection).
- **"Your choice last time"** means that session in that folder: every Giac
  scratchpad has the same name, so the name alone no longer counts.
- **A kernel started on its own** (not by a frontend) is named by its folder,
  and is no longer taken for "the session you started mathbuddy from".

- **llama.cpp models.** mathbuddy also talks to llama.cpp's `llama-server`,
  a single model or a router with several presets: `/llamacpp ADDRESS:PORT`
  adds one for good (`--llamacpp` for one run). Tool calls, thinking and
  streaming work as with Ollama; sampling is left to the server's preset.
- **A config file**, `~/.config/mathbuddy/config.toml`: Ollama's address,
  llama-servers, context window, thinking, hints, CPU. Options and
  `MATHBUDDY_*` variables still win; `mathbuddy doctor` shows what it read.
- **It remembers your choices.** The model you last picked is used again
  next time (never loaded on a router in place of another one), and the
  session you last picked comes first after one in the same tmux window.
  Without Ollama, a model a llama.cpp router already has loaded is used.
- **Model selector.** `/model` lists the models on Ollama and llama.cpp,
  numbered, with the one in use and the loaded ones marked; `/model N` or
  `/model name` switches. Before picking a llama.cpp model that would make
  the router unload another one, mathbuddy asks.
- **Menus.** `/model`, `/sessions` and the question before loading a model
  open pop-up menus: type to filter, arrows to choose. In the terminal's own
  colours.
  Outside an interactive terminal they print numbered lists as before.
- **mathbuddy-split is gone.** mathbuddy only connects to sessions you
  already have open; laying out panes is your terminal's or tmux's job. Run
  jotter and mathbuddy side by side however you like. (Delete a leftover
  `~/.local/bin/mathbuddy-split` link if you made one.)
- **Ctrl-C at the prompt** asks before quitting (Quit / Cancel); Ctrl-C
  twice quits. Cancel brings back what you had typed.
- `-m` and `MATHBUDDY_MODEL` find models on either server (router aliases
  too), and `mathbuddy doctor` checks the llama.cpp server when one is set.

## 0.2.0 (2026-11-10)

Upgrading: `cd ~/code/mathbuddy && git pull`. No new dependencies. Restart
any running mathbuddy; R sessions pick up the new bridge when they start.

- **Code that already ran isn't repeated.** The model no longer types out the
  code it just ran (slow on small machines). mathbuddy keeps that code itself,
  so `/insert`, `/copy` and `/show` work on it.
- **Plots are noticed.** When code draws a plot, mathbuddy says so ("drew 1
  plot in the RStudio Plots pane") instead of "no output", in R and in
  Python. It also notices plots you draw yourself in R.
- **Better plot habits and no invented values.** The model is told to set a
  single bar colour without a pointless legend, label axes, say what error
  bars show, never put a constant colour inside `aes()` (that makes a legend
  entry instead of red bars), and never assume data or parameters (such as
  an SD) it wasn't given.
- **Fewer failed attempts in R.** The model is told to load dplyr when it uses
  `%>%` (the most common failure on small models), and shown a correct
  example for a bar chart with error bars. It is also warned about a silent
  bug small models made: `data.frame(group = c("AAA", "BBB"), value =
  c(AAA, BBB))` recycles the labels and mixes up the groups, and that
  `geom_col()` stacks rows, so it needs one row per group.
- **`/insert` is shown** in the hint after an answer in RStudio (and only there).
- **`mathbuddy doctor`** checks a machine's setup (Ollama and model, R and the
  bridge, optional tools, sessions) and says how to fix what's missing.
- **`mathbuddy --version`** shows the commit too.
- **Docs:** [docs/setup.md](docs/setup.md) and [docs/usage.md](docs/usage.md).

## 0.1.0 (2026-11-10)

First version.

- Attaches to jotter and other Jupyter kernels, and to R and RStudio
  sessions through a small in-session bridge (`mathbuddy setup-r`).
- Finds the running sessions and suggests one, with the reason.
- Chat in the terminal with a local Ollama model; looks at your variables,
  runs code in your session only after you approve it.
- Picks the best model the Ollama server has; `--cpu`, `--ctx`, `--host`.
