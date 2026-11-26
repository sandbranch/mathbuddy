"""What the model sees, the tools it may use, and the answer loop."""

from __future__ import annotations

import json
import re

from .kernel import KernelWatch

CODE_BLOCK = re.compile(r"```[ \t]*([\w+-]*)[ \t]*\n(.*?)```", re.S)

SYSTEM = """You are mathbuddy, a small assistant looking over the user's shoulder \
while they work in a live {language} session in {frontend}. You can see what \
they run, as they run it, below.

How to help:
- Be brief. One short paragraph or a short list, then code if code is wanted.
- Check before guessing: use list_variables and inspect to see what a variable \
really is (columns, dtype, shape) before writing code against it.
- To compute an answer, call run_code. The user approves each run. Every \
number you report must come from run_code output you actually received in \
this conversation. Never estimate or invent a result.
- Never invent data or parameters (an SD, a sample size, a constant you were \
not given): compute them from the user's variables, or ask.
- After run_code succeeds, do NOT repeat its code in your answer: the user \
already saw it and can /insert or /copy it. Answer in one or two sentences: \
what it did and the result.
- If run_code is declined or not available, give ONE fenced code block that \
runs as-is in their session (using their existing variable names) and say \
they can run it with /run.
- For data entry (a pasted table, a list of values), turn it into the obvious \
structure (a data frame, an array) with sensible column names and units.
- If something is ambiguous or you are unsure, say so in one line.
{lang_hints}
{session}"""

LANG_HINTS = {
    "R": """
R specifics:
- Write R, in ```r blocks. Plots you draw go to their active graphics device \
(in RStudio, the Plots pane), so draw the plot itself; don't save it to a file unless asked.
- Installed helper packages: {packages}. Prefer ggplot2 for plots when it is \
installed (library(ggplot2) first), unless they ask for base graphics.
- Error bars: summarise first (mean and sd or se per group, computed from the \
data), then geom_col() + geom_errorbar(aes(ymin = ..., ymax = ...)), and say \
which spread they show (SD, SE or CI).
- One colour for all bars: set it outside aes(), e.g. geom_col(fill = "red"). \
NEVER write aes(fill = "red"): a constant inside aes() does not colour the \
bars red, it makes a legend entry named "red" in ggplot's default colour. \
Map fill or colour to a variable only when the colour should mean something.
- Label the axes with what they measure, units if known.
- print() the numbers you need to see; a ggplot object is drawn by printing it.
- Load every package you use: %>%, group_by() and summarise() need \
library(dplyr) first. Base R needs nothing, so prefer it for small summaries.
- Building one data frame from several vectors: make the group column with \
rep(), e.g. rep(c("AAA", "BBB"), times = c(length(AAA), length(BBB))). \
Never put c("AAA", "BBB") next to longer values: R silently recycles it and \
mixes up the groups.
- geom_col() draws one bar per row and stacks rows that share an x value, so \
summarise to exactly one row per group first (as in the example), or the bars \
come out as sums, not means.
- Example, bar chart of two vectors with ±1 SD error bars and one colour:
```r
library(ggplot2)
d <- data.frame(group = c("AAA", "BBB"),
                mean = c(mean(AAA), mean(BBB)),
                sd = c(sd(AAA), sd(BBB)))
ggplot(d, aes(x = group, y = mean)) +
  geom_col(fill = "red") +
  geom_errorbar(aes(ymin = mean - sd, ymax = mean + sd), width = 0.2) +
  labs(x = NULL, y = "Mean (±1 SD)")
```
""",
    "python": """
Python specifics: write Python, in ```python blocks; pandas and numpy for data.
""",
}

TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "list_variables",
            "description": "List the user's variables in the live session, with type and shape.",
            "parameters": {"type": "object", "properties": {}},
        },
    },
    {
        "type": "function",
        "function": {
            "name": "inspect",
            "description": "Describe one variable in the live session: type, shape or dim, "
            "columns, first rows, summary. Accepts a name, attribute access or a simple "
            "subscript (Python: df['x'], df.columns; R: df$x, lst[['a']]), but no function calls.",
            "parameters": {
                "type": "object",
                "properties": {"expr": {"type": "string", "description": "e.g. df, df.columns, df$x"}},
                "required": ["expr"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "run_code",
            "description": "Run code, in the session's language, in the user's live session and "
            "get its printed output back. The user is asked to approve every run. Variables it "
            "creates stay in their session, and plots appear in their plot pane. print() what "
            "you need to see.",
            "parameters": {
                "type": "object",
                "properties": {"code": {"type": "string", "description": "code to run"}},
                "required": ["code"],
            },
        },
    },
]
TOOL_NAMES = [t["function"]["name"] for t in TOOLS]


def session_block(watch: KernelWatch | None, n: int = 8) -> str:
    if watch is None:
        return "No session is attached yet, so you cannot see their work."
    cells = watch.recent(n)
    if not cells:
        return f"Attached to a {watch.language} session; nothing has been run since attaching."
    body = "\n\n".join(c.render() for c in cells)
    return f"What they ran most recently, oldest first:\n\n{body}"


def code_blocks(text: str) -> list[str]:
    return [m.group(2).rstrip() for m in CODE_BLOCK.finditer(text)]


class Assistant:
    def __init__(self, llm, max_history: int = 20, confirm=None, max_rounds: int = 6):
        """confirm(code) -> bool is asked before every run_code; None declines all."""
        self.llm = llm
        self.runs: list[str] = []  # code that ran successfully during the last ask()
        self.history: list[dict] = []
        self.max_history = max_history
        self.confirm = confirm
        self.max_rounds = max_rounds

    def _system(self, watch) -> dict:
        lang = watch.language if watch else "Python"
        frontend = getattr(watch, "frontend", None) or "a Jupyter frontend (jotter)"
        info = getattr(watch, "info", {}) or {}
        pk = info.get("packages") or []
        hints = LANG_HINTS.get(lang, "").format(packages=", ".join(pk) if pk else "none of ggplot2/dplyr/tidyr/data.table")
        return {"role": "system", "content": SYSTEM.format(language=lang, frontend=frontend,
                                                           lang_hints=hints, session=session_block(watch))}

    def _tool(self, watch, name: str, args: dict) -> str:
        if watch is None:
            return "no session attached"
        try:
            if name == "list_variables":
                return watch.list_variables()
            if name == "inspect":
                return watch.inspect(str(args.get("expr", "")))
            if name == "run_code":
                code = str(args.get("code", "")).strip()
                if not code:
                    return "no code given"
                if self.confirm is None or not self.confirm(code):
                    return ("The user declined this run. Do not report a computed result. "
                            "Give the code as a fenced block they can /run themselves.")
                status, out = watch.run(code)
                if status == "ok":
                    self.runs.append(code)
                out = out.strip() or "(ran without printing anything; print() what you need to see)"
                if len(out) > 3000:
                    out = out[:1500] + "\n...(truncated)...\n" + out[-1500:]
                return f"status: {status}\n{out}"
        except Exception as e:  # a busy kernel, a dead kernel, a timeout
            return f"tool failed: {e}"
        return f"There is no tool named {name!r}. Available tools: {', '.join(TOOL_NAMES)}."

    def ask(self, watch, text: str, on_token=None, on_tool=None, on_thinking=None, cancel=None) -> str:
        self.runs = []
        self.history.append({"role": "user", "content": text})
        turn: list[dict] = []
        tools = TOOLS if (watch is not None and watch.can_inspect) else None
        answer = ""
        for rnd in range(self.max_rounds + 1):
            msgs = [self._system(watch)] + self.history + turn
            last = rnd == self.max_rounds
            if last:  # out of tool rounds: make it answer with what it has
                msgs.append({"role": "user", "content": "Stop calling tools now and answer with what you have."})
            r = self.llm.chat(msgs, tools=None if last else tools, on_token=on_token,
                              on_thinking=on_thinking, cancel=cancel)
            if cancel is not None and cancel.is_set():
                answer = r["content"]
                break
            if not r["tool_calls"] or last:
                answer = r["content"]
                break
            turn.append({"role": "assistant", "content": r["content"], "tool_calls": r["tool_calls"]})
            for call in r["tool_calls"]:
                fn = call.get("function", {})
                name, args = fn.get("name", ""), fn.get("arguments") or {}
                if isinstance(args, str):
                    try:
                        args = json.loads(args)
                    except Exception:
                        args = {}
                if on_tool:
                    on_tool(name, args)
                result = self._tool(watch, name, args)
                if on_tool:
                    on_tool("result", {"text": result})
                turn.append({"role": "tool", "tool_name": name, "content": result})
        self.history.append({"role": "assistant", "content": answer})
        self.history = self.history[-self.max_history :]
        return answer

    def hint(self, watch, cell) -> str:
        """One-line nudge after an error. No tools, no history, short."""
        msgs = [
            self._system(watch),
            {
                "role": "user",
                "content": f"Cell In [{cell.count}] just raised an error. In ONE sentence of at "
                "most 25 words, say the likely cause and the fix. No code block.",
            },
        ]
        r = self.llm.chat(msgs, max_tokens=200)
        return " ".join(r["content"].split())

    def clear(self):
        self.history.clear()
