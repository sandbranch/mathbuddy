# Using mathbuddy

mathbuddy is a chat in a terminal, next to the session you work in. You type
questions in plain language; it answers in text. It sees what you run as you
run it, it can look at your variables, and it can run code in your session,
but only after showing you the code and getting a yes.

Setting it up on a machine: [setup.md](setup.md).

## Starting it

**In RStudio:** open the Terminal tab (next to Console) and run `mathbuddy`.
It attaches to that RStudio's R session by itself:

```
attached: R · RStudio · assay-project (you started mathbuddy from inside this RStudio)
```

**With jotter** (Jupyter notebooks in the terminal):

```sh
mathbuddy-split notes.ipynb        # jotter left, mathbuddy right, in tmux
```

**Anywhere else:** `mathbuddy` attaches to the best session it can find.

Useful options (all of them: `mathbuddy --help`):

| | |
|---|---|
| `--cpu` | keep the model off the graphics card (when the card is busy) |
| `-m MODEL` | use a specific Ollama model |
| `--host ADDRESS:11434` | use another machine's Ollama |
| `-s WORDS` | attach to a particular session, e.g. `-s rstudio` or `-s demo.ipynb` |
| `-y` | run code without asking each time (be sure you want this) |
| `--no-hints` | no automatic hint when something you run fails |

## Which session it's attached to

The bottom bar always shows it, e.g. `R · RStudio · assay-project · 12 seen`.
When several sessions are running, mathbuddy picks one and tells you why:
first the one you started it from inside, then one next to it in the same
tmux window, one in the same folder, then the newest.

```
» /sessions
 *[1] R · RStudio · assay-project  (started 12 min ago)  ← you started mathbuddy from inside this RStudio
  [2] Python · jotter · demo.ipynb  (started 3 min ago)
» /attach 2
```

`/attach` also takes words: `/attach jotter`, `/attach demo`. When a new
session starts, mathbuddy mentions it with its number.

## Asking

Just type and press Enter:

```
» what's in df?
[inspect(df)]
df has 6 rows: g (AAA or BBB) and y (numeric, 3.9 to 5.6).

» plot AAA and BBB in a bar diagram with error bars, red bars
mathbuddy wants to run this in your session:
  │ library(ggplot2)
  │ ...
run it? [y]es / [n]o / [a]lways this session: y
  → (drew 1 plot in the RStudio Plots pane)
Red bars show the means; the error bars are ±1 standard deviation.
(code that ran: /insert, /copy, /show)
```

- **Follow-ups work**: "now use standard error", "make it grey". It remembers
  the conversation (`/clear` forgets it) and also sees what you run yourself.
- **Paste data** straight in: a table from a spreadsheet, a list of numbers,
  an error message, several lines at once. "Make this a data frame called
  runs" is a typical small job.
- **Lines in grey** in square brackets, like `[inspect(df)]`, are mathbuddy
  looking at your data. That never changes anything.
- **When something you run fails**, mathbuddy offers a one-line hint by
  itself, marked `hint`. `/hints off` stops that.

## Approving code

Before anything runs in your session, mathbuddy shows the full code and asks:

- `y` runs it once, `n` declines (it then gives you the code to run yourself),
  `a` approves everything for the rest of this session.
- **Read the code before you say yes, and look at the result.** Small models
  (like the 4b one on a 4 GB card) are usually right but not always: in tests
  one assumed an SD of 0.1 instead of computing it, and one drew bars of sums
  instead of means without any error. Both were visible in the code. For
  answers you need to trust, use a bigger model (`--host` to a bigger machine).
- Code it runs changes your real variables, and plots go to your Plots pane,
  but the code isn't saved anywhere. To keep it:

| | |
|---|---|
| `/insert` | RStudio: put the code into your open script at the cursor |
| `/console` | RStudio: run it in the console as if you typed it, so it's in your History |
| `/copy` | copy it to the clipboard, e.g. to paste into a notebook cell |
| `/show` | print it again |
| `/run` | run a code block from an answer (one it suggested but didn't run) |

With several blocks, add a number: `/insert 2`.

## All the commands

`/help` lists these in mathbuddy itself.

| | |
|---|---|
| `/sessions`, `/attach [N or words]` | list sessions, switch |
| `/vars` | your variables |
| `/cells` | what mathbuddy has seen you run |
| `/run`, `/insert`, `/console`, `/copy`, `/show` `[n]` | see above |
| `/model [name]` | show or switch the model |
| `/hints on` or `/hints off` | automatic hints on errors |
| `/clear` | forget the conversation |
| `/quit` | quit (or Ctrl-D) |

Ctrl-C stops an answer in progress. Alt-Enter types a newline without sending.

## Asking once, from a script or another terminal

```sh
mathbuddy "what columns does df have?"
mathbuddy -s rstudio "summarise the last fit"
```

It prints the answer and exits. It won't run code this way unless you add `-y`.

## Good to know

- **It's all local.** The model runs in Ollama on your machine (or one you
  point it at with `--host`); nothing goes to the cloud.
- **Speed.** The first answer after starting is the slowest, because the model
  has to load. Expect answers from a few seconds to about a minute, depending
  on the machine and the model.
- **While your own code runs**, mathbuddy can't look at your variables; it
  waits until R or the kernel is free again.
- **What it can do per session type:** looking at variables and running code
  work in Python (jotter, Jupyter) and in R (RStudio, terminal R). For other
  Jupyter kernels (Octave, Wolfram, IRkernel) it watches and gives code for you
  to run.
- **Check the setup** any time with `mathbuddy doctor`.
