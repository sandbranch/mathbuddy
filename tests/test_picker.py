"""The pop-up picker's filter and keys, and /split's guards (no tmux needed)."""

from __future__ import annotations

from types import SimpleNamespace

from prompt_toolkit.application import create_app_session
from prompt_toolkit.input import create_pipe_input
from prompt_toolkit.output import DummyOutput

from mathbuddy import picker
from mathbuddy.app import App, kernel_specs
from mathbuddy.picker import Item, matches


def test_filter_is_fuzzy_and_covers_group_and_detail():
    it = Item("qwen3.5:latest", detail="loaded", group="ollama · 127.0.0.1:11434")
    assert matches("qw35", it) and matches("QWEN", it) and matches("loaded", it) and matches("ollama", it)
    assert not matches("bonsai", it) and not matches("53qw", it)


def run_picker(keys: str, items, **kw):
    with create_pipe_input() as inp:
        inp.send_text(keys)
        with create_app_session(input=inp, output=DummyOutput()):
            return picker.pick("Test", items, **kw)


ITEMS = [Item("alpha", 1, group="g1"), Item("beta", 2, group="g1"), Item("gamma", 3, group="g2", current=True)]


def test_picker_starts_on_current_and_enter_returns_value():
    assert run_picker("\r", ITEMS) == 3


def test_picker_arrows_stop_at_the_ends():
    down, up = "\x1b[B", "\x1b[A"
    assert run_picker(down * 5 + "\r", ITEMS) == 3  # already last: stays there, no wrap to alpha
    assert run_picker(up * 5 + "\r", ITEMS) == 1


def test_picker_typing_filters_and_escape_cancels():
    assert run_picker("bet\r", ITEMS) == 2
    assert run_picker("zzz\r", ITEMS) is None
    assert run_picker("\x1b", ITEMS) is None


def test_yes_no_defaults_to_no():
    with create_pipe_input() as inp:
        inp.send_text("\r")
        with create_app_session(input=inp, output=DummyOutput()):
            assert picker.ask_yes_no("Load?", "Yes", "No") is False


def test_kernel_specs_lists_python3_first():
    specs = kernel_specs()
    assert next(iter(specs)) == "python3"
    assert all({"display_name", "language"} <= set(v) for v in specs.values())


def make_app():
    args = SimpleNamespace(model=None, host="127.0.0.1:9", cpu=False, threads=None, think=False, ctx=8192,
                           llamacpp=None, yes=False, no_hints=True, kernel=None)
    app = App(args)
    out = []
    app.out = out.append
    return app, out


def test_split_needs_tmux(monkeypatch):
    monkeypatch.delenv("TMUX", raising=False)
    app, out = make_app()
    app.command("/split")
    assert "needs tmux" in out[-1]


def test_split_rejects_unknown_target(monkeypatch):
    monkeypatch.setenv("TMUX", "/tmp/fake,1,0")
    app, out = make_app()
    app.command("/split nosuchkernel")
    assert "/split takes r, a notebook" in out[-1] and "python3" in out[-1]


def run_quit(keys: str) -> bool:
    with create_pipe_input() as inp:
        inp.send_text(keys)
        with create_app_session(input=inp, output=DummyOutput()):
            return picker.confirm_quit()


def test_quit_dialog_keys():
    assert run_quit("\r") is True  # Yep! is chosen first, like crush
    assert run_quit("\x03") is True  # Ctrl-C again quits
    assert run_quit("\x1b[C\r") is False  # right arrow to Nope
    assert run_quit("\t \x1b[D\r") is False  # tab to Nope then space confirms
    assert run_quit("n") is False and run_quit("y") is True and run_quit("\x1b") is False


def test_enter_tmux_only_outside_tmux_in_a_terminal(monkeypatch):
    from mathbuddy import app as appmod

    calls = []
    monkeypatch.setattr(appmod.os, "execvp", lambda *a: calls.append(a))
    monkeypatch.setattr(appmod.shutil, "which", lambda n: "/usr/bin/" + n)
    monkeypatch.setattr(appmod.sys.stdin, "isatty", lambda: True, raising=False)
    monkeypatch.setattr(appmod.sys.stdout, "isatty", lambda: True, raising=False)
    monkeypatch.setenv("MATHBUDDY_LLAMACPP_HOST", "127.0.0.1:8081")
    args = SimpleNamespace(no_tmux=False)
    monkeypatch.setenv("TMUX", "/tmp/x,1,0")
    appmod.enter_tmux(["notes.ipynb"], args)
    assert calls == []  # already inside tmux
    monkeypatch.delenv("TMUX")
    appmod.enter_tmux(["notes.ipynb"], SimpleNamespace(no_tmux=True))
    assert calls == []  # --no-tmux
    appmod.enter_tmux(["notes.ipynb"], args)
    (prog, argv), = calls
    assert prog == "tmux" and argv[:2] == ["tmux", "new-session"]
    assert "MATHBUDDY_LLAMACPP_HOST=127.0.0.1:8081" in argv and any(a.startswith("MATHBUDDY_FROM_PID=") for a in argv)
    assert argv[argv.index(";") - 1].endswith("-m mathbuddy notes.ipynb") and argv[-2:] == ["mouse", "on"]


def test_rank_counts_where_we_were_started_from(monkeypatch):
    from mathbuddy import sessions as sm

    s = sm.Session("r-bridge", "/x/r-1.json", "R", "RStudio", "proj", 0.0, pid=4242)
    monkeypatch.setattr(sm, "_tmux_window_pane_pids", lambda: set())
    monkeypatch.setattr(sm, "ancestors", lambda pid: {111: [111, 4242, 1], 4242: [4242, 1]}.get(pid, [pid, 1]))
    monkeypatch.setenv("MATHBUDDY_FROM_PID", "111")  # the tmux client, under RStudio's shell
    (top,) = sm.rank([s])
    assert top.score == 3 and "started mathbuddy from inside this RStudio" in top.reason
