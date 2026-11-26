# Changelog

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
