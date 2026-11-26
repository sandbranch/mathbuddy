"""The pop-up picker's filter and keys, and the quit dialog."""

from __future__ import annotations

from prompt_toolkit.application import create_app_session
from prompt_toolkit.input import create_pipe_input
from prompt_toolkit.output import DummyOutput

from mathbuddy import picker
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
