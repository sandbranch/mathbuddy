# mathbuddy

A small local LLM looking over your shoulder while you work in a live
[jotter](https://samox73.github.io/jotter/) or Jupyter session, or in R
(RStudio or terminal R).

It attaches to the same session you are working in, sees everything you run
(code, output, errors) as you run it, and sits in the pane next to you for
the small jobs: turning a pasted table into a DataFrame, writing a short
fit, explaining an error. It can look at your actual variables before it
answers, and it can run code in your session to compute an answer, but only
after showing you the code and getting a yes.

```
┌ jotter ─────────────────────────────┐┌ mathbuddy ──────────────────────────┐
│ In [2]                              ││  hint [2]: you used rates['k'] but  │
│ rates['ln_k'] = np.log(rates['k'])  ││  the column is named k_per_s.       │
│ KeyError: 'k'                       ││ » fit an Arrhenius equation, Ea?    │
│                                     ││ [inspect(rates.columns)]            │
│                                     ││ mathbuddy wants to run this ...     │
│                                     ││   → Activation Energy: 68.81 kJ/mol │
└─────────────────────────────────────┘└─────────────────────────────────────┘
```

## Quick start

Set up once per machine (details and checks: [docs/setup.md](docs/setup.md)):

```sh
git clone https://github.com/sandbranch/mathbuddy ~/code/mathbuddy
cd ~/code/mathbuddy && uv tool install --editable .
ollama pull qwen3.5            # or qwen3.5:4b-q4_K_M on a 4 GB graphics card
mathbuddy setup-r              # only if you use R or RStudio
mathbuddy doctor               # checks everything, says how to fix what's missing
```

Then:

- **RStudio:** open the Terminal tab and run `mathbuddy`.
- **jotter or another Jupyter frontend:** open your notebook as usual, then run
  `mathbuddy` in another terminal (or a tmux pane next to it). It finds the
  session by itself; `/sessions` picks another.
- Type a question, press Enter. `/help` lists the commands.

How to use it, with examples: [docs/usage.md](docs/usage.md).
What changed in each version: [CHANGELOG.md](CHANGELOG.md).

## Models tested

With no `-m`, mathbuddy uses the first of qwen3.5 (9b), gemma4, qwen3:8b,
qwen3.5:4b, qwen3:4b, gpt-oss:20b that the Ollama server has, and says
which. Tested with real R sessions on 2026-11-10:

| model | verdict |
|---|---|
| qwen3.5 (9b) | reliable: inspects first, computes, correct numbers |
| qwen3.5:4b (3.3 GB) | fits a 4 GB card. With 0.2.0's R guidance, a plot request takes 14 to 37 s on a GTX 1650, mostly in one attempt. Usually right, but not always: in 4 runs of the same chart, 1 plotted raw values with `geom_col()` (stacked sums, not means), with no error. Read the code before approving and look at the chart; for answers you must trust, use a 9b model (`--host` to a bigger machine) |
| qwen3:4b | only works with thinking on (mathbuddy turns it on, hidden); right, but about 2 min per answer |
| qwen3.5:2b | not usable: broken R, functions that don't exist |

Measured speed:

| machine | model | mode | first answer | follow-up | writing |
|---|---|---|---|---|---|
| Ryzen 9 5900X desktop, card busy | qwen3.5 | `--cpu --threads 8` | hint about 25 s; a fit with code 1 to 3 min | | |
| i7-9750H laptop, GTX 1650 4 GB | qwen3:4b | GPU, 8k window | 18 s | 7 s | 17.9 tok/s |
| same laptop | qwen3:4b | GPU, 16k window | 71 s (incl. 31 s first load) | 9 s | 14.0 tok/s |
| same laptop | qwen3:4b | `--cpu` (6 threads) | 48 s | 14 s | 8.8 tok/s |

On a 4 GB card the context window decides whether the model fits: at 16k,
half of qwen3:4b is pushed off the GPU and prompt reading drops from about
166 to 43 tokens/s.

## How it works, and what that means

* jotter writes a standard kernel connection file to
  `$XDG_RUNTIME_DIR/jupyter/jotter-<uuid>.json`. mathbuddy connects as a
  second client. A kernel broadcasts every cell's code and output to all of
  its clients (IOPub), so watching needs nothing from jotter.
* jotter leaves those files behind when it exits, so liveness is checked on
  the kernel's heartbeat socket, which also answers while a cell is busy.
* Looking at a variable uses `user_expressions` on a silent request: no
  output in jotter, no history entry. Only names, attributes and simple
  subscripts are allowed, never calls, so looking can't change anything.
* Code that mathbuddy runs (with your yes, or by `/run`) changes your real
  namespace and is recorded in the kernel's `In[]` history, which is why
  jotter's counter can jump a few numbers. It is **not** a notebook cell, so
  for reproducibility, `/copy` the code into a real cell when it matters.
* On attach it reads the current session's input history (not older
  sessions'), so it knows what you've done before it arrived.
* The kernel runs one thing at a time: while a long cell runs, the helper's
  questions about variables wait (and time out after 20 s).
* Variable inspection and `run_code` work for Python kernels and for R via
  the bridge. Watching works for every Jupyter kernel (Octave, IRkernel R,
  Wolfram); for those it answers from what it has seen and gives code for
  you to run.

## R bridge

R has no kernel to attach to, so `mathbuddy/r/bridge.R` runs inside each R
session (loaded from `~/.Rprofile` by `mathbuddy setup-r`). It records each
top-level command (task callback), each uncaught error (a global calling
handler, registered from a `later` callback because R refuses it while any
handler is on the stack), and each new plot page (the `plot.new` and
`grid.newpage` hooks). It serves these, the variable list, read-only
inspection and approved runs on 127.0.0.1 at a random port, only to requests
carrying the token from `$XDG_RUNTIME_DIR/mathbuddy/r-<pid>.json` (mode 0600,
removed when R exits). In RStudio, `/insert` and `/console` use `rstudioapi`.

## Development

```sh
python3 -m pytest tests     # real ipykernel and real R, no model or GPU needed
```

Versions: the number lives in `mathbuddy/__init__.py` (pyproject reads it
from there). For a release: bump it, add a section to `CHANGELOG.md`, commit,
`git tag -a vX.Y.Z`, push the tag, and make a GitHub release from it.
`mathbuddy --version` also shows the commit when installed from a checkout.

## License

Apache-2.0, see [LICENSE](LICENSE).
