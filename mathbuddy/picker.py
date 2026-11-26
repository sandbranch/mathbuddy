"""Pop-up pickers in the style of crush's dialogs: a rounded box with a
gradient title, a filter you type into, grouped items, the chosen row on the
primary colour, and the keys at the bottom. Drawn below the prompt and erased
when done, so the conversation above stays as it was.

Colours are crush's default theme (charmtone): Charple, Dolly, Butter, Smoke,
Oyster, Char, Guac.
"""

from __future__ import annotations

import os
import shutil
import sys
from dataclasses import dataclass

PRIMARY, SECONDARY = (0x6B, 0x50, 0xFF), (0xFF, 0x60, 0xFF)  # Charple, Dolly
ON_PRIMARY = "#FFFAF1"  # Butter
FG, SUBTLE, MOST_SUBTLE, RULE, GOOD = "#ECEBF0", "#BFBCC8", "#605F6B", "#3A3943", "#12C78F"
MAX_WIDTH, MAX_ROWS = 73, 10


@dataclass
class Item:
    label: str
    value: object = None
    detail: str = ""  # dim text after the label, e.g. "loaded"
    group: str = ""  # items with the same group sit under one heading
    current: bool = False  # marked with a check


def can_pick() -> bool:
    return sys.stdin.isatty() and sys.__stdout__.isatty()


def matches(query: str, item: Item) -> bool:
    """Every typed character appears in order (fuzzy, like crush's filter)."""
    hay = f"{item.label} {item.detail} {item.group}".lower()
    pos = 0
    for ch in query.lower().replace(" ", ""):
        pos = hay.find(ch, pos) + 1
        if pos == 0:
            return False
    return True


def _hex(c) -> str:
    return "#%02x%02x%02x" % c


def _gradient(text: str, style_extra: str = "") -> list[tuple[str, str]]:
    n = max(len(text) - 1, 1)
    out = []
    for i, ch in enumerate(text):
        t = i / n
        c = tuple(round(a + (b - a) * t) for a, b in zip(PRIMARY, SECONDARY))
        out.append((f"fg:{_hex(c)} {style_extra}".strip(), ch))
    return out


def pick(title: str, items: list[Item], placeholder: str = "Type to filter", start: int | None = None):
    """Show the picker; return the chosen item's value, or None on Esc/Ctrl-C."""
    from prompt_toolkit.application import Application
    from prompt_toolkit.key_binding import KeyBindings
    from prompt_toolkit.keys import Keys
    from prompt_toolkit.layout import Layout, Window
    from prompt_toolkit.layout.controls import FormattedTextControl

    if not items:
        return None
    state = {"query": "", "sel": 0, "top": 0}
    if start is None:
        start = next((i for i, it in enumerate(items) if it.current), 0)
    state["sel"] = start

    def visible() -> list[Item]:
        return [it for it in items if matches(state["query"], it)]

    def move(d: int):
        vis = visible()
        if vis:
            state["sel"] = min(max(state["sel"] + d, 0), len(vis) - 1)  # no wrapping: safer for yes/no

    def render():
        cols = shutil.get_terminal_size((80, 24)).columns
        w = max(24, min(MAX_WIDTH, cols - 1))
        inner = w - 4  # "│ " ... " │"
        vis = visible()
        state["sel"] = min(state["sel"], max(len(vis) - 1, 0))

        # rows: group headings and items, with the index of each item's row
        rows, item_row, last_group = [], {}, None
        for i, it in enumerate(vis):
            if it.group and it.group != last_group:
                rows.append(("group", it.group))
                last_group = it.group
            item_row[i] = len(rows)
            rows.append(("item", i))
        sel_row = item_row.get(state["sel"], 0)
        if sel_row < state["top"]:
            state["top"] = max(sel_row - 1, 0) if sel_row and rows[sel_row - 1][0] == "group" else sel_row
        if sel_row >= state["top"] + MAX_ROWS:
            state["top"] = sel_row - MAX_ROWS + 1
        shown = rows[state["top"]: state["top"] + MAX_ROWS]

        border = f"fg:{RULE}"
        frags: list[tuple[str, str]] = []

        def line(parts: list[tuple[str, str]], used: int):
            frags.append((border, "│ "))
            frags.extend(parts)
            frags.append(("", " " * max(inner - used, 0)))
            frags.append((border, " │\n"))

        frags.append((border, "╭" + "─" * (w - 2) + "╮\n"))
        t = title[: inner - 2]
        fill = max(inner - len(t) - 1, 0)
        line(_gradient(t, "bold") + [("", " ")] + _gradient("╱" * fill), len(t) + 1 + fill)
        q = state["query"]
        if q:
            shown_q = q[-(inner - 3):]
            line([(f"fg:{_hex(PRIMARY)} bold", "> "), (f"fg:{FG}", shown_q), ("reverse", " ")], 3 + len(shown_q))
        else:
            ph = placeholder[: inner - 3]
            line([(f"fg:{_hex(PRIMARY)} bold", "> "), ("reverse", " "), (f"fg:{MOST_SUBTLE}", ph)], 3 + len(ph))
        line([], 0)
        if not vis:
            msg = "nothing matches"[:inner]
            line([(f"fg:{MOST_SUBTLE} italic", msg)], len(msg))
        for kind, v in shown:
            if kind == "group":
                g = v[: inner - 2]
                line([(f"fg:{SUBTLE}", g), ("", " "), (f"fg:{RULE}", "─" * max(inner - len(g) - 1, 0))], inner)
                continue
            it = vis[v]
            mark = "✓ " if it.current else "  "
            label = it.label[: inner - 4]
            detail = it.detail[: max(inner - 4 - len(label) - 2, 0)]
            gap = max(inner - 2 - len(mark) - len(label) - len(detail), 1)
            text_len = 1 + len(mark) + len(label) + gap + len(detail) + 1
            if v == state["sel"]:
                st = f"bg:{_hex(PRIMARY)} fg:{ON_PRIMARY}"
                line([(st, " " + mark + label + " " * gap), (st + " italic", detail), (st, " ")], text_len)
            else:
                line([("", " "), (f"fg:{GOOD}", mark), (f"fg:{FG}", label), ("", " " * gap),
                      (f"fg:{MOST_SUBTLE}", detail), ("", " ")], text_len)
        more = len(rows) - (state["top"] + len(shown))
        line([(f"fg:{MOST_SUBTLE}", f"  ↓ {more} more" if more > 0 else "")], len(f"  ↓ {more} more") if more > 0 else 0)
        keys = [("↑↓", "choose"), ("enter", "confirm"), ("esc", "cancel")]
        hp, used = [], 0
        for i, (k, d) in enumerate(keys):
            sep = " • " if i else ""
            hp += [(f"fg:{RULE}", sep), (f"fg:{SUBTLE}", k), (f"fg:{MOST_SUBTLE}", " " + d)]
            used += len(sep) + len(k) + 1 + len(d)
        line(hp, used)
        frags.append((border, "╰" + "─" * (w - 2) + "╯"))
        return frags

    kb = KeyBindings()

    @kb.add("up")
    @kb.add("c-p")
    def _(e):
        move(-1)

    @kb.add("down")
    @kb.add("c-n")
    @kb.add("tab")
    def _(e):
        move(1)

    @kb.add("pageup")
    def _(e):
        move(-MAX_ROWS)

    @kb.add("pagedown")
    def _(e):
        move(MAX_ROWS)

    @kb.add("enter")
    def _(e):
        vis = visible()
        e.app.exit(result=vis[state["sel"]].value if vis else None)

    @kb.add("escape", eager=True)
    @kb.add("c-c")
    @kb.add("c-d")
    def _(e):
        e.app.exit(result=None)

    @kb.add("backspace")
    def _(e):
        state["query"] = state["query"][:-1]
        state["sel"] = 0

    @kb.add("c-u")
    def _(e):
        state["query"], state["sel"] = "", 0

    @kb.add(Keys.Any)
    def _(e):
        ch = e.data
        if ch.isprintable():
            state["query"] += ch
            state["sel"] = 0

    app = Application(layout=Layout(Window(FormattedTextControl(render, show_cursor=False), dont_extend_height=True)),
                      key_bindings=kb, full_screen=False, erase_when_done=True, mouse_support=False,
                      color_depth=_color_depth())
    return app.run()


def ask_yes_no(title: str, yes: str, no: str) -> bool:
    """A two-item picker for a question; Esc counts as no."""
    return bool(pick(title, [Item(yes, True), Item(no, False)], placeholder="Choose", start=1))


def _color_depth():
    from prompt_toolkit.output.color_depth import ColorDepth

    return ColorDepth.TRUE_COLOR if os.environ.get("COLORTERM") in ("truecolor", "24bit") else None


BUTTON_ON, BUTTON_OFF = "#FF60FF", RULE  # crush: Dolly for the chosen button, Char for the other


def confirm_quit() -> bool:
    """crush's quit dialog: Yep! / Nope, Yep! chosen. Ctrl-C again (or y) quits."""
    from prompt_toolkit.application import Application
    from prompt_toolkit.key_binding import KeyBindings
    from prompt_toolkit.layout import Layout, Window
    from prompt_toolkit.layout.controls import FormattedTextControl

    question = "Are you sure you want to quit?"
    hints = ["To quit without confirmation", "press ctrl+c twice."]
    state = {"no": False}

    def render():
        buttons = [("Yep!", not state["no"]), ("Nope", state["no"])]
        bw = sum(len(t) + 6 for t, _ in buttons) + 1  # padding 3 each side, one space between
        inner = max(len(question), bw, *(len(h) for h in hints)) + 4  # padding 2 each side
        border = f"fg:{_hex(PRIMARY)}"
        frags: list[tuple[str, str]] = [(border, "╭" + "─" * inner + "╮\n")]

        def centred(parts: list[tuple[str, str]], width: int):
            left = (inner - width + 1) // 2
            frags.append((border, "│"))
            frags.append(("", " " * left))
            frags.extend(parts)
            frags.append(("", " " * (inner - width - left)))
            frags.append((border, "│\n"))

        centred([], 0)
        centred([(f"fg:{FG}", question)], len(question))
        centred([], 0)
        parts = []
        for i, (text, on) in enumerate(buttons):
            if i:
                parts.append(("", " "))
            parts.append((f"bg:{BUTTON_ON} fg:{ON_PRIMARY}" if on else f"bg:{BUTTON_OFF} fg:{FG}", f"   {text}   "))
        centred(parts, bw)
        centred([], 0)
        for h in hints:
            centred([(f"fg:{MOST_SUBTLE}", h)], len(h))
        centred([], 0)
        frags.append((border, "╰" + "─" * inner + "╯"))
        return frags

    kb = KeyBindings()

    @kb.add("left")
    @kb.add("right")
    @kb.add("tab")
    @kb.add("s-tab")
    def _(e):
        state["no"] = not state["no"]

    @kb.add("enter")
    @kb.add(" ")
    def _(e):
        e.app.exit(result=not state["no"])

    @kb.add("y")
    @kb.add("Y")
    @kb.add("c-c")
    def _(e):
        e.app.exit(result=True)

    @kb.add("n")
    @kb.add("N")
    @kb.add("escape", eager=True)
    def _(e):
        e.app.exit(result=False)

    app = Application(layout=Layout(Window(FormattedTextControl(render, show_cursor=False), dont_extend_height=True)),
                      key_bindings=kb, full_screen=False, erase_when_done=True, color_depth=_color_depth())
    return bool(app.run())
