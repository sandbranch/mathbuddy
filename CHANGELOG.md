# Changelog

## Unreleased

- **llama.cpp models.** mathbuddy also talks to llama.cpp's `llama-server`,
  a single model or a router with several presets: `--llamacpp ADDRESS:PORT`
  or `MATHBUDDY_LLAMACPP_HOST`. Tool calls, thinking and streaming work as
  with Ollama; sampling is left to the server's preset.
- **Model selector.** `/model` lists the models on Ollama and llama.cpp,
  numbered, with the one in use and the loaded ones marked; `/model N` or
  `/model name` switches. Before picking a llama.cpp model that would make
  the router unload another one, mathbuddy asks.
- **Menus.** `/model`, `/sessions` and the question before loading a model
  open pop-up menus in the style of crush: type to filter, arrows to choose.
  Outside an interactive terminal they print numbered lists as before.
- **`/split`** starts jotter (with a kernel you pick) or R in a tmux pane next
  to mathbuddy and attaches to it. `/split notes.ipynb`, `/split giac` and
  `/split r` skip the menus.
- **One command.** `mathbuddy notes.ipynb` replaces `mathbuddy-split`
  (removed; delete your `~/.local/bin/mathbuddy-split` link). Started outside
  tmux, mathbuddy now moves itself into a new tmux session so `/split` works;
  `--no-tmux` or `MATHBUDDY_NO_TMUX=1` keeps it where it is.
- **Ctrl-C at the prompt** opens crush's quit dialog (Yep! / Nope); Ctrl-C
  twice quits. Nope brings back what you had typed.
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
