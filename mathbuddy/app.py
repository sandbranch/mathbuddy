"""mathbuddy: a small local LLM looking over your shoulder in a jotter session."""

from __future__ import annotations

import argparse
import base64
import os
import shutil
import subprocess
import sys
import threading
import time

from . import __version__
from .assistant import Assistant, code_blocks
from .kernel import KernelWatch, kernel_alive
from .llm import LlamaCpp, Models, Ollama
from .rbridge import BRIDGE_R
from .sessions import Session, discover, pick, rank

HELP = """\
Type a question and press Enter. Pasting multi-line text (a table, a traceback) works; \
Alt-Enter adds a newline by hand.

The model can look at your variables on its own. Before it runs any code in your
session it shows the code and asks: y(es), n(o), or a(lways for this session).

  /run [n]       run code block n from the last answer (default: the last one) in the session
  /show [n]      print code block n again
  /copy [n]      copy code block n to the clipboard, to paste into a jotter cell
  /vars          list the session's variables
  /cells         show the cells mathbuddy has seen
  /sessions      list the sessions I can attach to, best suggestion first
  /attach [N|words]  attach to session N from that list, or one matching words
                 (rstudio, jotter, a notebook or project name); no argument: my suggestion
  /insert [n]    RStudio: put code block n into your open script at the cursor
  /console [n]   RStudio: send code block n to the console, as if you typed it
  /model         list the models on your Ollama and llama.cpp servers, numbered
  /model N|name  switch to model N from that list, or by name
  /hints on|off  one-line hints when a cell raises an error
  /clear         forget the conversation (not the cells)
  /help, /quit   (Ctrl-C stops an answer, Ctrl-D quits)
"""

AMUX = os.environ.get("AMUX_ENV") == "1"


def amux_status(state: str, msg: str = ""):
    """amux's Program Status Protocol (OSC 7501); a no-op outside amux."""
    if not AMUX:
        return
    b = base64.b64encode(msg.encode()).decode()
    sys.__stdout__.write(f"\x1b]7501;state={state}:msg={b}\x1b\\")
    sys.__stdout__.flush()


def copy_to_clipboard(text: str) -> str:
    for cmd in (["wl-copy"], ["xclip", "-selection", "clipboard"], ["xsel", "-ib"]):
        if shutil.which(cmd[0]):
            try:
                subprocess.run(cmd, input=text.encode(), check=True, timeout=5)
                return cmd[0]
            except Exception:
                continue
    # OSC 52: the terminal (and tmux with set-clipboard on) takes it from here.
    b = base64.b64encode(text.encode()).decode()
    seq = f"\x1b]52;c;{b}\x07"
    if os.environ.get("TMUX"):
        seq = "\x1bPtmux;" + seq.replace("\x1b", "\x1b\x1b") + "\x1b\\"
    sys.__stdout__.write(seq)
    sys.__stdout__.flush()
    return "OSC 52"


class App:
    def __init__(self, args):
        self.args = args
        servers = [Ollama(PREFERRED_MODELS[0], host=args.host, cpu=args.cpu,
                          threads=args.threads, think=args.think, num_ctx=args.ctx)]
        if args.llamacpp:
            servers.append(LlamaCpp(args.llamacpp, think=args.think))
        self.llm = Models(servers)
        self.model_listing: list[tuple[object, str, bool]] = []
        self.assistant = Assistant(self.llm, confirm=self.confirm_run)
        self.always_run = args.yes
        self.interactive_mode = False
        self.watch = None  # KernelWatch or RBridge
        self.session: Session | None = None
        self.listing: list[Session] = []
        self.known: set[str] = set()
        self.hints = not args.no_hints
        self.busy = threading.Lock()
        self.blocks: list[str] = []
        self.stop = threading.Event()
        self.out = print  # replaced by prompt_toolkit's safe printer in interactive mode

    # -- sessions and attaching ----------------------------------------------

    def find_sessions(self, probe_r: bool = True) -> list[Session]:
        pats = [self.args.kernel] if self.args.kernel else None
        return rank(discover(pats, probe_r=probe_r))

    def print_sessions(self, found: list[Session]):
        if not found:
            self.out("(no sessions found: start jotter, or R/RStudio with the bridge loaded; "
                     "`mathbuddy setup-r` sets that up)")
            return
        for i, s in enumerate(found, 1):
            mark = "*" if self.watch and self.watch.cf == s.path else " "
            self.out(f" {mark}[{i}] {s.line()}")

    def attach(self, target: str | None = None, quiet: bool = False) -> bool:
        if self.args.connection_file:
            cf = self.args.connection_file
            if self.watch is None and kernel_alive(cf):
                self.watch = KernelWatch(cf, on_cell_done=self.on_cell_done)
                self.session = None
                self.out(f"attached: {self.watch.kernel_name} kernel, {os.path.basename(cf)}")
            return self.watch is not None
        found = self.find_sessions()
        self.listing = found
        self.known = {s.path for s in found}
        s = pick(found, target)
        if s is None:
            if not quiet:
                if target:
                    self.out(f"(no session matches {target!r}; /sessions lists them)")
                else:
                    self.out("(no sessions yet: start jotter, or R/RStudio with the bridge loaded; "
                             "I'll attach when one appears)")
            return False
        if not self._attach_session(s, quiet=quiet):
            return False
        if len(found) > 1 and not target:
            self.out("sessions running (/attach N to switch):")
            self.print_sessions(found)
        return True

    def _attach_session(self, s: Session, quiet: bool = False) -> bool:
        if self.watch and self.watch.cf == s.path:
            if not quiet:
                self.out(f"(already attached to {s.title()})")
            return True
        try:
            new = s.connect(on_cell_done=self.on_cell_done)
        except Exception as e:
            self.out(f"(could not attach to {s.title()}: {e})")
            return False
        if self.watch:
            self.watch.close()
        self.watch, self.session = new, s
        seen = len(new.recent(100))
        self.out(f"attached: {s.title()}" + (f" ({s.reason})" if s.reason else "")
                 + (f", {seen} earlier commands seen" if seen else ""))
        return True

    def attach_loop(self):
        """Attach when a session appears, notice new ones, drop dead ones."""
        while not self.stop.wait(2.0):
            try:
                if self.args.connection_file:
                    if self.watch is None:
                        self.attach(quiet=True)
                    continue
                if self.watch is not None and not self.watch.alive():
                    old = self.session.title() if self.session else os.path.basename(self.watch.cf)
                    self.watch.close()
                    self.watch, self.session = None, None
                    self.out(f"(session {old} went away; waiting for the next one, /sessions to pick)")
                if self.watch is None:
                    self.attach(self.args.session, quiet=True)
                    continue
                found = self.find_sessions(probe_r=False)
                fresh = [s for s in found if s.path not in self.known]
                if fresh:
                    self.listing = found
                    self.known |= {s.path for s in found}
                    for s in fresh:
                        self.out(f"(new session: [{found.index(s) + 1}] {s.title()}"
                                 f"{', ' + s.reason if s.reason and s.score else ''}; /attach {found.index(s) + 1})")
            except Exception as e:
                self.out(f"(attach error: {e})")

    # -- watching -------------------------------------------------------------

    def on_cell_done(self, cell):
        if not (self.hints and cell.error):
            return
        if not self.busy.acquire(blocking=False):
            return  # mid-answer; don't pile on

        def run():
            try:
                amux_status("working", "hint")
                h = self.assistant.hint(self.watch, cell)
                if h:
                    self.out(f"\n  hint [{cell.count}]: {h}")
            except Exception as e:
                self.out(f"\n  (hint failed: {e})")
            finally:
                amux_status("idle")
                self.busy.release()

        threading.Thread(target=run, daemon=True).start()

    # -- commands ---------------------------------------------------------

    def pick(self, arg: str) -> str | None:
        if not self.blocks:
            self.out("(no code in the last answer)")
            return None
        try:
            n = int(arg) if arg else len(self.blocks)
            return self.blocks[n - 1]
        except (ValueError, IndexError):
            self.out(f"(code blocks are 1 to {len(self.blocks)})")
            return None

    def command(self, line: str) -> bool:
        """Returns False to quit."""
        cmd, _, arg = line[1:].partition(" ")
        arg = arg.strip()
        if cmd in ("q", "quit", "exit"):
            return False
        if cmd in ("h", "help", "?"):
            self.out(HELP)
        elif cmd == "show":
            code = self.pick(arg)
            if code:
                self.out(code)
        elif cmd == "copy":
            code = self.pick(arg)
            if code:
                self.out(f"(copied via {copy_to_clipboard(code)})")
        elif cmd == "run":
            code = self.pick(arg)
            if code and self.need_kernel():
                keep = ("/insert puts it in your script" if getattr(self.watch, "info", {}).get("rstudio_api")
                        else "/copy it to keep it")
                self.out(f"(running in your session: it changes your variables, but it isn't saved "
                         f"in your notebook or script; {keep})")
                try:
                    status, text = self.watch.run(code)
                    self.out(text.rstrip() or f"(done, status {status}, no output)")
                except Exception as e:
                    self.out(f"(run failed: {e})")
        elif cmd == "vars":
            if self.need_kernel():
                self.out(self.watch.list_variables())
        elif cmd == "cells":
            if self.need_kernel():
                cells = self.watch.recent(20)
                self.out("\n\n".join(c.render(400, 300) for c in cells) or "(none yet)")
        elif cmd == "sessions":
            self.listing = self.find_sessions()
            self.known |= {s.path for s in self.listing}
            self.print_sessions(self.listing)
            if self.listing:
                self.out("(/attach N, or /attach with words like: rstudio, jotter, demo.ipynb)")
        elif cmd == "attach":
            if arg.isdigit() and self.listing:  # numbers refer to the last list shown
                i = int(arg) - 1
                if 0 <= i < len(self.listing):
                    arg = None
                    self._attach_session(self.listing[i])
                    return True
            self.attach(arg or None)
        elif cmd in ("insert", "console"):
            code = self.pick(arg)
            if code and self.need_kernel():
                fn = getattr(self.watch, "insert" if cmd == "insert" else "to_console", None)
                if fn is None:
                    self.out(f"(/{cmd} works in RStudio sessions; use /copy here)")
                else:
                    try:
                        self.out(f"({fn(code)})")
                    except Exception as e:
                        self.out(f"(/{cmd} failed: {e})")
        elif cmd in ("model", "models"):
            if arg:
                self.switch_model(arg)
            else:
                self.print_models()
        elif cmd == "hints":
            self.hints = arg != "off"
            self.out(f"(hints {'on' if self.hints else 'off'})")
        elif cmd == "clear":
            self.assistant.clear()
            self.out("(conversation cleared)")
        else:
            self.out(f"(unknown command /{cmd}; /help lists them)")
        return True

    def choose_model(self):
        """-m picks a model on any configured server. Without it, use the best
        one Ollama has: never a llama.cpp model, since asking a llama.cpp router
        for a model it hasn't loaded makes it load one, maybe in place of another."""
        if self.args.model:
            hit = self.locate_model(self.args.model)
            if hit:
                if hit[0].kind == "llama.cpp":
                    self.confirm_llamacpp_load(*hit, ask=False)  # named with -m: warn, don't ask
                self.llm.use(*hit)
                self._think_default()
            else:
                self.out(f"(model {self.args.model} isn't on any of your servers; /model lists what they have)")
            return
        try:
            names = self.llm.servers[0].models()
        except Exception as e:
            self.out(f"(can't reach Ollama at {self.llm.servers[0].base}: {e}"
                     + ("; /model lists the llama.cpp models" if len(self.llm.servers) > 1 else "") + ")")
            return
        for m in PREFERRED_MODELS:
            found = find_model(names, m)
            if found:
                self.llm.model = found
                self.out(f"(model: {found}, the best one this Ollama has; -m picks another)")
                self._think_default()
                return
        self.out(f"(none of {', '.join(PREFERRED_MODELS)} is on {self.llm.base}; it has: "
                 f"{', '.join(names) or 'none'}. Pick one with -m, or `ollama pull qwen3.5`)")

    def locate_model(self, want: str):
        """(server, name) for a model name on any server, Ollama first."""
        for s in self.llm.servers:
            try:
                name = find_model(s.models(), want) if s.kind == "ollama" else s.find(want)
            except Exception:
                continue
            if name:
                return s, name
        return None

    def print_models(self):
        self.model_listing = []
        lines = []
        for server, models, err in self.llm.catalog():
            lines.append(f"{server.kind} at {server.base}" + (f": not reachable ({err})" if models is None else ""))
            for name, loaded in models or []:
                self.model_listing.append((server, name, loaded))
                cur = server is self.llm.server and name == self.llm.model
                lines.append(f"  {'*' if cur else ' '}[{len(self.model_listing)}] {name}"
                             + ("   (loaded)" if loaded else ""))
            if models == []:
                lines.append("  (no models)")
        self.out("\n".join(lines))
        self.out("(* in use; /model N or /model NAME switches"
                 + ("; a llama.cpp model that isn't loaded gets loaded when you ask something" if
                    any(s.kind == "llama.cpp" for s in self.llm.servers) else "") + ")")

    def switch_model(self, arg: str):
        hit = None
        if arg.isdigit() and self.model_listing:  # numbers refer to the last list shown
            i = int(arg) - 1
            if 0 <= i < len(self.model_listing):
                server, name, _ = self.model_listing[i]
                hit = (server, name)
        if hit is None:
            hit = self.locate_model(arg)
        if hit is None:
            self.out(f"(no model {arg!r} on your servers; /model lists them)")
            return
        server, name = hit
        if server.kind == "llama.cpp" and not self.confirm_llamacpp_load(server, name):
            self.out(f"(staying with {self.llm.label()})")
            return
        self.llm.use(server, name)
        self._think_default()
        note = ""
        if server.kind == "llama.cpp" and (self.args.cpu or self.args.threads):
            note = "; --cpu and --threads apply to Ollama only, llama.cpp runs as its preset says"
        self.out(f"(model: {self.llm.label()}{note})")

    def confirm_llamacpp_load(self, server, name: str, ask: bool = True) -> bool:
        """A llama.cpp router loads a model on first use and may unload another
        to make room, which would cut into whatever that one is doing. Ask."""
        try:
            cat = server.catalog()
        except Exception:
            return True
        if dict(cat).get(name):
            return True
        others = [n for n, loaded in cat if loaded and n != name]
        if not others:
            return True
        self.out(f"({name} isn't loaded on {server.base}. Asking it something loads it, and the server "
                 f"may unload {', '.join(others)} to make room, interrupting anything using it.)")
        if not ask or not self.interactive_mode:
            return True  # named on the command line: the user asked for exactly this
        from prompt_toolkit import prompt

        try:
            a = prompt(f"switch to {name} anyway? [y]es / [n]o: ").strip().lower()
        except (KeyboardInterrupt, EOFError):
            return False
        return a in ("y", "yes")

    def _think_default(self):
        if not self.llm.think and self.llm.model.startswith(THINK_ALWAYS):
            self.llm.think = True  # hidden unless --think; see THINK_ALWAYS

    def need_kernel(self) -> bool:
        if self.watch is None:
            self.out("(no session attached yet; /sessions lists them)")
            return False
        return True

    # -- asking -------------------------------------------------------------

    def confirm_run(self, code: str) -> bool:
        """Asked before the model runs anything in the user's kernel."""
        sys.stdout.write("\n\x1b[1mmathbuddy wants to run this in your session:\x1b[0m\n")
        sys.stdout.write("\n".join("  │ " + ln for ln in code.splitlines()) + "\n")
        sys.stdout.flush()
        if self.always_run:
            sys.stdout.write("\x1b[2m(approved: always-run is on)\x1b[0m\n")
            return True
        if not self.interactive_mode:
            sys.stdout.write("\x1b[2m(declined: pass --yes to allow runs from the command line)\x1b[0m\n")
            return False
        from prompt_toolkit import prompt

        while True:
            try:
                a = prompt("run it? [y]es / [n]o / [a]lways this session: ").strip().lower()
            except (KeyboardInterrupt, EOFError):
                return False
            if a in ("y", "yes", ""):
                return True
            if a in ("n", "no"):
                return False
            if a in ("a", "always"):
                self.always_run = True
                return True

    def ask(self, text: str, stream=sys.stdout):
        with self.busy:
            amux_status("working", text[:60])
            cancel = threading.Event()
            state = {"thinking": False, "col0": True}

            def write(s):
                if s:
                    stream.write(s)
                    stream.flush()
                    state["col0"] = s.endswith("\n")

            def newline():
                if not state["col0"]:
                    write("\n")

            def tok(t):
                if state["thinking"]:
                    write("\x1b[0m\n")
                    state["thinking"] = False
                write(t)

            def think(t):
                if not state["thinking"]:
                    write("\x1b[2m")
                    state["thinking"] = True
                write(t)

            def think_quiet(t):  # the model is thinking but we don't show it
                if not state.get("said_thinking"):
                    state["said_thinking"] = True
                    write("\x1b[2m(thinking...)\x1b[0m\n")

            def tool(name, args):
                newline()
                if name == "result":
                    text = args["text"]
                    if text.startswith("status:"):  # run_code output: show the real numbers
                        lines = [ln for ln in text.splitlines() if ln.strip()]
                        if lines[0] == "status: error" and len(lines) > 8:
                            shown = lines[:1] + ["  ..."] + lines[-6:]  # a traceback's point is its end
                        else:
                            shown = lines[:14] + (["  ..."] if len(lines) > 14 else [])
                        write("\x1b[2m" + "\n".join("  → " + ln for ln in shown) + "\x1b[0m\n")
                    return
                if name == "run_code":
                    return  # confirm_run shows the code
                a = args.get("expr", "")
                write(f"\x1b[2m[{name}{'(' + a + ')' if a else ''}]\x1b[0m\n")

            try:
                answer = self.assistant.ask(self.watch, text, on_token=tok, on_tool=tool,
                                            on_thinking=think if self.args.think else think_quiet, cancel=cancel)
                stream.write("\x1b[0m\n")
                # Code that already ran counts as a block too, so the model needn't repeat it.
                blocks = list(self.assistant.runs)
                blocks += [b for b in code_blocks(answer) if b.strip() not in {r.strip() for r in blocks}]
                self.blocks = blocks
                if blocks:
                    n = len(blocks)
                    in_rstudio = bool(getattr(self.watch, "info", {}).get("rstudio_api"))
                    cmds = (["/insert"] if in_rstudio else []) + ["/copy", "/show"]
                    cmds += [] if self.assistant.runs and n == len(self.assistant.runs) else ["/run"]
                    what = "code that ran" if self.assistant.runs and n == len(self.assistant.runs) else \
                        f"{n} code block{'s' if n > 1 else ''}"
                    stream.write(f"\x1b[2m({what}: {', '.join(cmds)}{' with a number' if n > 1 else ''})\x1b[0m\n")
            except KeyboardInterrupt:
                cancel.set()
                if self.assistant.history and self.assistant.history[-1]["role"] == "user":
                    self.assistant.history.pop()
                stream.write("\x1b[0m\n(stopped)\n")
            except Exception as e:
                if self.assistant.history and self.assistant.history[-1]["role"] == "user":
                    self.assistant.history.pop()
                stream.write(f"\x1b[0m\n(error: {e})\n")
            finally:
                stream.flush()
                amux_status("idle")

    # -- main loops ---------------------------------------------------------

    def toolbar(self):
        if self.watch:
            what = self.session.title() if self.session else f"{self.watch.language} kernel"
            k = f"{what} · {len(self.watch.recent(100))} seen"
        else:
            k = "no session (/sessions)"
        return (f" mathbuddy · {self.llm.label()} · {k}"
                f" · hints {'on' if self.hints else 'off'} · /help ")

    def interactive(self):
        from prompt_toolkit import PromptSession, print_formatted_text
        from prompt_toolkit.history import FileHistory
        from prompt_toolkit.key_binding import KeyBindings
        from prompt_toolkit.patch_stdout import patch_stdout

        kb = KeyBindings()

        @kb.add("escape", "enter")
        def _(event):
            event.current_buffer.insert_text("\n")

        hist = os.path.join(os.environ.get("XDG_STATE_HOME", os.path.expanduser("~/.local/state")), "mathbuddy")
        os.makedirs(hist, exist_ok=True)
        session = PromptSession(history=FileHistory(os.path.join(hist, "history")), key_bindings=kb,
                                bottom_toolbar=self.toolbar, refresh_interval=1.0)
        self.interactive_mode = True
        with patch_stdout(raw=True):
            self.out = lambda s: print_formatted_text(s)
            print(f"{version_string()}: /help for commands")
            self.choose_model()
            self.attach(self.args.session)
            threading.Thread(target=self.attach_loop, daemon=True).start()
            amux_status("idle")
            while True:
                try:
                    line = session.prompt("» ").strip()
                except KeyboardInterrupt:
                    continue
                except EOFError:
                    break
                if not line:
                    continue
                if line.startswith("/"):
                    if not self.command(line):
                        break
                else:
                    self.ask(line)
        self.stop.set()
        if self.watch:
            self.watch.close()

    def once(self, question: str) -> int:
        self.choose_model()
        self.attach(self.args.session)
        self.ask(question)
        if self.args.run and self.blocks:
            print("--- /run", file=sys.stderr)
            self.command("/run")
        if self.watch:
            self.watch.close()
        return 0


# Tool-calling models, best first, used when no model is given. Tested on
# 2026-11-10: qwen3.5 (9b) reliable; qwen3.5:4b usually right, sometimes
# assumes values; qwen3:4b only works with thinking on (slow); qwen3.5:2b
# writes broken R and calls functions that don't exist, so it isn't listed.
PREFERRED_MODELS = ["qwen3.5:latest", "qwen3.5:9b", "gemma4", "qwen3:8b", "qwen3.5:4b", "qwen3:4b", "gpt-oss:20b"]
# Hybrid thinking models whose "think: false" Ollama 0.34 doesn't honour: with
# it off they reason in the answer and fumble tool calls; with it on they work.
THINK_ALWAYS = ("qwen3:",)


def names_or_empty(llm) -> list[str]:
    try:
        return llm.models()
    except Exception:
        return []


def find_model(names: list[str], want: str) -> str | None:
    """The server's name for `want`: exact, `want:latest`, or a quantisation
    variant like qwen3.5:4b-q4_K_M for qwen3.5:4b."""
    for n in names:
        if n == want or n == want + ":latest" or (":" in want and n.startswith(want + "-")):
            return n
    return None


def has_model(names: list[str], model: str) -> bool:
    return find_model(names, model) is not None


RPROFILE_MARK = "# mathbuddy bridge"
SOURCE_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def version_string() -> str:
    """Version, plus the git commit when running from a checkout (editable install)."""
    s = f"mathbuddy {__version__}"
    if os.path.isdir(os.path.join(SOURCE_DIR, ".git")):
        try:
            git = lambda *a: subprocess.run(["git", "-C", SOURCE_DIR, *a], capture_output=True,
                                            text=True, timeout=3).stdout.strip()
            sha = git("rev-parse", "--short", "HEAD")
            dirty = git("status", "--porcelain", "--untracked-files=no")
            if sha:
                s += f" ({sha}{', with local changes' if dirty else ''})"
        except Exception:
            pass
    return s


def rprofile_path() -> str:
    return os.environ.get("R_PROFILE_USER") or os.path.expanduser("~/.Rprofile")


def doctor(argv: list[str]) -> int:
    """Check this machine's setup and say how to fix what's missing."""
    import re as _re

    problems = 0

    def row(state, text, fix=""):
        nonlocal problems
        if state == "fix":
            problems += 1
        print(f"  {state:<5} {text}" + (f"\n        -> {fix}" if fix else ""))

    print(f"{version_string()}, from {SOURCE_DIR}")

    llama_host = os.environ.get("MATHBUDDY_LLAMACPP_HOST")
    print("Ollama (the model)")
    llm = Ollama(PREFERRED_MODELS[0], host=os.environ.get("MATHBUDDY_OLLAMA_HOST"))
    try:
        import json as _json
        import urllib.request as _u

        with _u.urlopen(llm.base + "/api/version", timeout=5) as r:
            ver = _json.load(r).get("version", "?")
        row("ok", f"reachable at {llm.base} (Ollama {ver})")
        names = llm.models()
        want = os.environ.get("MATHBUDDY_MODEL")
        if want and (found := find_model(names, want)):
            row("ok", f"MATHBUDDY_MODEL={want}")
        elif want and llama_host:
            row("--", f"MATHBUDDY_MODEL={want} is not on Ollama (checked under llama.cpp below)")
        elif want:
            row("fix", f"MATHBUDDY_MODEL={want} is not on this server", f"ollama pull {want}")
        else:
            best = next((f for m in PREFERRED_MODELS if (f := find_model(names, m))), None)
            row("ok" if best else "fix", f"model: {best} (the best one this server has)" if best else
                f"none of {', '.join(PREFERRED_MODELS)} is on this server",
                "" if best else "ollama pull qwen3.5   (or qwen3.5:4b for a 4 GB graphics card)")
    except Exception as e:
        row("fix", f"cannot reach Ollama at {llm.base} ({e})",
            "start it (systemctl --user start ollama), or point at another machine: "
            "mathbuddy --host ADDRESS:11434")

    print("llama.cpp (optional, more models)")
    if not llama_host:
        row("--", "not set up (to use llama-server models: MATHBUDDY_LLAMACPP_HOST=ADDRESS:PORT)")
    else:
        lc = LlamaCpp(llama_host)
        try:
            cat = lc.catalog()
            loaded = [n for n, on in cat if on]
            row("ok", f"reachable at {lc.base}: {len(cat)} model(s)"
                + (f", loaded: {', '.join(loaded)}" if loaded else ", none loaded"))
            want = os.environ.get("MATHBUDDY_MODEL")
            if want and not find_model(names_or_empty(llm), want):
                hit = lc.find(want)
                row("ok" if hit else "fix", f"MATHBUDDY_MODEL={want}" + ("" if hit else " is on neither server"),
                    "" if hit else "pick one from /model in mathbuddy")
        except Exception as e:
            row("fix", f"cannot reach llama-server at {lc.base} ({getattr(e, 'reason', e)})",
                "start llama-server, or fix MATHBUDDY_LLAMACPP_HOST")

    print("R and RStudio (optional)")
    if not shutil.which("Rscript"):
        row("--", "R is not installed (only needed for R/RStudio sessions)")
    else:
        code = ('for (p in c("httpuv","later","jsonlite","rstudioapi","ggplot2")) '
                'cat(p, requireNamespace(p, quietly = TRUE), "\\n"); cat("R", R.version$major, R.version$minor, "\\n")')
        out = subprocess.run(["Rscript", "--no-init-file", "-e", code], capture_output=True, text=True, timeout=60).stdout
        have = dict(line.split()[:2] for line in out.splitlines() if len(line.split()) >= 2 and line.split()[0] != "R")
        rver = next((ln for ln in out.splitlines() if ln.startswith("R ")), "R ?").split()
        row("ok", f"R {'.'.join(rver[1:3])}")
        need = [p for p in ("httpuv", "later", "jsonlite") if have.get(p) != "TRUE"]
        row("fix" if need else "ok", "bridge packages: " + ("missing " + ", ".join(need) if need else "httpuv, later, jsonlite"),
            f'in R: install.packages(c({", ".join(repr(p) for p in need)}))' if need else "")
        for p, why in (("rstudioapi", "/insert and /console in RStudio"), ("ggplot2", "nicer plots")):
            row("ok" if have.get(p) == "TRUE" else "--", f"{p} ({why})" + ("" if have.get(p) == "TRUE" else ": not installed"))
        prof = rprofile_path()
        text = open(prof).read() if os.path.exists(prof) else ""
        line = next((ln for ln in text.splitlines() if RPROFILE_MARK in ln), None)
        if line is None:
            row("fix", f"{prof} does not load the bridge, so R sessions won't show up", "mathbuddy setup-r")
        else:
            m = _re.search(r'source\("([^"]+)"\)', line)
            path = m.group(1) if m else ""
            if not os.path.exists(path):
                row("fix", f"{prof} loads the bridge from {path}, which doesn't exist (moved?)",
                    f"delete that line from {prof}, then: mathbuddy setup-r")
            else:
                row("ok", f"{prof} loads the bridge" + ("" if os.path.samefile(path, BRIDGE_R) else f" from {path}"))

    print("Other tools (optional)")
    row("ok" if shutil.which("jotter") else "--", "jotter (Jupyter notebooks in the terminal)" +
        ("" if shutil.which("jotter") else ": not installed"))
    row("ok" if shutil.which("tmux") else "--", "tmux (for mathbuddy-split)" + ("" if shutil.which("tmux") else ": not installed"))
    amux_cfg = os.path.expanduser("~/.amux/config.json")
    if os.path.exists(amux_cfg):
        row("ok" if "mathbuddy" in open(amux_cfg).read() else "--", "amux knows mathbuddy" if "mathbuddy" in
            open(amux_cfg).read() else "amux is set up but has no mathbuddy agent (see docs/setup.md)")

    print("Sessions I can see right now")
    found = rank(discover())
    if not found:
        print("  (none: start jotter, or R/RStudio after setup-r)")
    for i, s in enumerate(found, 1):
        print(f"  [{i}] {s.line()}")
    print("all good" if not problems else f"{problems} thing{'s' if problems > 1 else ''} to fix (see -> above)")
    return 1 if problems else 0


def setup_r(argv: list[str]) -> int:
    """Add the bridge to ~/.Rprofile (or $R_PROFILE_USER), once."""
    line = f'if (interactive()) try(source("{BRIDGE_R}"), silent = TRUE)  {RPROFILE_MARK}'
    if argv[:1] in (["--print"], ["-p"]):
        print(line)
        return 0
    path = rprofile_path()
    old = open(path).read() if os.path.exists(path) else ""
    if RPROFILE_MARK in old:
        print(f"already set up in {path}")
        return 0
    with open(path, "a") as f:
        f.write(("" if not old or old.endswith("\n") else "\n") + line + "\n")
    print(f"added to {path}:\n  {line}\n"
          "New interactive R sessions (RStudio, terminal R) now load the bridge. For one that is\n"
          f"already running, paste this into its console:\n  source(\"{BRIDGE_R}\")\n"
          "Note: a project's own .Rprofile replaces ~/.Rprofile; add the line there too if you use one.")
    return 0


def main(argv=None) -> int:
    env = os.environ.get
    p = argparse.ArgumentParser(
        prog="mathbuddy", description=__doc__,
        epilog="other commands:\n  mathbuddy doctor     check this machine's setup, say how to fix what's missing\n"
               "  mathbuddy setup-r    make R and RStudio sessions visible to mathbuddy (~/.Rprofile)\n\n"
               "docs: docs/setup.md and docs/usage.md in the mathbuddy folder",
        formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("question", nargs="*", help="ask once and exit (otherwise interactive)")
    p.add_argument("-m", "--model", default=env("MATHBUDDY_MODEL"),
                   help=f"model: on Ollama, or on llama.cpp with --llamacpp (default: the first of "
                   f"{', '.join(PREFERRED_MODELS)} that Ollama has). In the chat, /model lists and switches")
    p.add_argument("--host", default=env("MATHBUDDY_OLLAMA_HOST"), help="Ollama address (default $OLLAMA_HOST)")
    p.add_argument("--llamacpp", default=env("MATHBUDDY_LLAMACPP_HOST"), metavar="HOST",
                   help="llama.cpp llama-server address, a single model or a router (e.g. 127.0.0.1:8081; "
                   "also MATHBUDDY_LLAMACPP_HOST). Its models show up in /model")
    p.add_argument("--cpu", action="store_true", default=env("MATHBUDDY_CPU") == "1",
                   help="keep the model off the GPU (num_gpu=0); also MATHBUDDY_CPU=1")
    p.add_argument("--threads", type=int, default=int(env("MATHBUDDY_THREADS", "0")) or None,
                   help="CPU threads for the model (with --cpu)")
    p.add_argument("--ctx", type=int, default=int(env("MATHBUDDY_CTX", "8192")),
                   help="model context window in tokens (default 8192; also MATHBUDDY_CTX). Bigger costs "
                   "memory: on a 4 GB card, 16384 pushes half of qwen3:4b off the GPU")
    p.add_argument("--think", action="store_true", help="let thinking models think (slower, shown dimmed)")
    p.add_argument("-s", "--session", help="attach to this session: a number from --list, or words to match "
                   "(rstudio, jotter, a notebook or project name)")
    p.add_argument("-l", "--list", action="store_true", help="list the sessions I can attach to, and exit")
    p.add_argument("--kernel", help="only Jupyter connection files matching this pattern (e.g. 'kernel-*.json')")
    p.add_argument("-f", "--connection-file", help="attach to this exact Jupyter connection file")
    p.add_argument("--no-hints", action="store_true", help="no automatic hints on errors")
    p.add_argument("--run", action="store_true", help="with a one-shot question: also run the last code block")
    p.add_argument("-y", "--yes", action="store_true",
                   help="let the model run code in the kernel without asking each time")
    p.add_argument("-V", "--version", action="version", version=version_string())
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["setup-r"]:
        return setup_r(argv[1:])
    if argv[:1] == ["doctor"]:
        return doctor(argv[1:])
    args = p.parse_args(argv)
    app = App(args)
    if args.list:
        found = app.find_sessions()
        app.print_sessions(found)
        return 0
    if args.question:
        return app.once(" ".join(args.question))
    app.interactive()
    return 0


if __name__ == "__main__":
    sys.exit(main())
