# Setting up mathbuddy on a machine

About ten minutes on a Linux machine that already has Python and git. Each
step says how to check it worked. At the end, `mathbuddy doctor` checks the
whole setup and tells you what, if anything, is missing.

What you need:

- Linux, Python 3.10 or newer, git.
- [uv](https://docs.astral.sh/uv/), which installs mathbuddy in its own
  environment. Check with `uv --version`.
- [Ollama](https://ollama.com/download), which runs the model. Check with
  `ollama --version`.
- Optional: R (for R and RStudio sessions), [jotter](https://samox73.github.io/jotter/)
  (for Jupyter notebooks in the terminal), tmux.

## 1. Install mathbuddy

```sh
git clone https://github.com/sandbranch/mathbuddy ~/code/mathbuddy
cd ~/code/mathbuddy
uv tool install --editable .
ln -sf ~/code/mathbuddy/bin/mathbuddy-split ~/.local/bin/mathbuddy-split
```

Check: `mathbuddy --version` prints the version and the commit, for example
`mathbuddy 0.2.0 (a1b2c3d)`. If the command isn't found, `~/.local/bin` is
not on your PATH; uv prints how to fix that (`uv tool update-shell`).

`--editable` means the installed command runs the code in `~/code/mathbuddy`
directly, so updating is a `git pull` (see [Updating](#updating)).

## 2. Give it a model

mathbuddy talks to Ollama. Pick the model by the machine's hardware:

| the machine has | pull | what to expect |
|---|---|---|
| a graphics card with 8 GB or more, or a fast CPU and 16 GB+ RAM | `ollama pull qwen3.5` (6.6 GB) | reliable; checks your data first, computes, correct numbers |
| a 4 GB graphics card (like a GTX 1650) | `ollama pull qwen3.5:4b-q4_K_M` (3.3 GB) | 25 to 50 s per answer on the GPU; usually right, sometimes assumes a value (you see it in the approval prompt) |
| little memory, no usable card | use another machine's Ollama (below) | |

You don't have to tell mathbuddy which model you pulled: with no `-m` it
uses the best one the server has, and says which. To check:
`mathbuddy doctor` prints the model it will use.

**Using another machine's Ollama** (for example a desktop with a bigger
graphics card, from a laptop):

```sh
mathbuddy --host gpu-desktop:11434
```

That works only if Ollama on that machine listens on an address you can
reach: set `OLLAMA_HOST` in its systemd service to an address on your
network or VPN. Ollama's default is 127.0.0.1, which only the machine
itself can reach. To make it permanent on the machine you
run mathbuddy on, set `MATHBUDDY_OLLAMA_HOST` in your shell profile.

**GPU or CPU:** Ollama uses the graphics card by itself when it can. When
the card is busy with something else, `mathbuddy --cpu` keeps the model off
it entirely (`--threads N` caps the CPU threads). On a 4 GB card, keep the
context window at the default 8192 (`--ctx`): at 16384, half of the model
no longer fits on the card and it gets about 4 times slower.

## 3. R and RStudio (optional)

R has no kernel for mathbuddy to attach to, so a small bridge runs inside
each R session. It needs three R packages:

```r
install.packages(c("httpuv", "later", "jsonlite"))   # in R
install.packages(c("rstudioapi", "ggplot2"))         # optional: /insert and /console in RStudio, nicer plots
```

Then make every interactive R session load the bridge:

```sh
mathbuddy setup-r
```

This adds one line to `~/.Rprofile` (it never replaces what is there).
Check: start a new R session; `mathbuddy --list` should show it. For an R
session that was already running, paste this into its console once:
`source("~/code/mathbuddy/mathbuddy/r/bridge.R")`.

A project's own `.Rprofile` replaces `~/.Rprofile` for that project. If you
use one, add the line there too (`mathbuddy setup-r --print` prints it).

What the bridge does, so there are no surprises: it records the commands
you run and their errors, and answers mathbuddy on 127.0.0.1 (this machine
only) at a random port, and only to requests carrying a random token from a
file only you can read. It runs code only when mathbuddy sends it, which
mathbuddy does only after you said yes. It adds nothing to your
environment.

## 4. Optional extras

- **jotter**, for Jupyter notebooks in the terminal: mathbuddy finds jotter's
  kernels by itself. `mathbuddy-split notes.ipynb` opens both side by side
  in tmux.
- **amux**: to run mathbuddy as an amux agent, add it to `~/.amux/config.json`:

  ```json
  { "assistants": { "mathbuddy": { "command": "mathbuddy", "interrupt_count": 1 } } }
  ```

## 5. Check everything

```sh
mathbuddy doctor
```

It checks Ollama and the model, R and the bridge, the optional tools, and
lists the sessions it can see. Every problem comes with the command that
fixes it. It ends with `all good`, or with how many things need fixing.

## Updating

```sh
cd ~/code/mathbuddy && git pull
```

The next `mathbuddy` you start runs the new code. If the release notes in
[CHANGELOG.md](../CHANGELOG.md) say the dependencies changed, also run
`uv tool install --editable --force .` in that folder. `mathbuddy --version`
shows which version and commit a machine is on.

## Removing it

```sh
uv tool uninstall mathbuddy
rm ~/.local/bin/mathbuddy-split
```

and delete the line ending in `# mathbuddy bridge` from `~/.Rprofile`.

## When something doesn't work

- **`command not found: mathbuddy` over SSH** (`ssh host 'mathbuddy ...'`):
  non-interactive shells often don't read the file that puts `~/.local/bin`
  on the PATH. Use the full path: `ssh host '~/.local/bin/mathbuddy doctor'`.
- **An R or RStudio session doesn't show up:** run `mathbuddy doctor`. Usually
  the bridge line is missing (`mathbuddy setup-r`), the session was started
  before setup (paste the `source(...)` line), or the project has its own
  `.Rprofile`.
- **"cannot reach Ollama":** start it (`systemctl --user start ollama` if it
  runs as a user service), or use another machine's with `--host`.
- **Answers are very slow:** check the model fits the hardware (step 2). On a
  busy graphics card, try `--cpu`. The first answer after starting is always
  the slowest, because the model has to load.
