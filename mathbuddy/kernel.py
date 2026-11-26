"""Attach to a running Jupyter kernel (jotter's by default), watch it, inspect it.

Everything here speaks the plain Jupyter protocol, so it works with any
frontend that leaves a connection file behind. The kernel broadcasts every
cell's code, output and error on IOPub to all connected clients; we are just
one more client listening.
"""

from __future__ import annotations

import ast
import glob
import os
import queue
import re
import threading
import time
from collections import OrderedDict
from dataclasses import dataclass, field

import jupyter_client

ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
# What the model may ask to inspect: a name, attribute access, simple subscripts.
# No calls, so an "inspection" can't change the user's state.
INSPECTABLE = re.compile(r"^[A-Za-z_]\w*(\.[A-Za-z_]\w*|\[[^\[\]()]{1,40}\])*$")

MAX_CELLS = 60


def strip_ansi(s: str) -> str:
    return ANSI.sub("", s)


def runtime_dirs() -> list[str]:
    dirs = []
    xdg = os.environ.get("XDG_RUNTIME_DIR")
    if xdg:
        dirs.append(os.path.join(xdg, "jupyter"))
    try:
        from jupyter_core.paths import jupyter_runtime_dir

        dirs.append(jupyter_runtime_dir())
    except Exception:
        pass
    out = []
    for d in dirs:
        if d not in out and os.path.isdir(d):
            out.append(d)
    return out


def candidate_files(pattern: str) -> list[str]:
    files = []
    for d in runtime_dirs():
        files += glob.glob(os.path.join(d, pattern))
    return sorted(set(files), key=os.path.getmtime, reverse=True)


def kernel_alive(cf: str, timeout: float = 1.5) -> bool:
    """Ping the kernel's heartbeat socket. The heartbeat is answered by its own
    thread in the kernel, so a kernel busy with a long cell still counts as
    alive, and a stale connection file (jotter leaves them behind) does not."""
    import json

    import zmq

    try:
        with open(cf) as f:
            info = json.load(f)
        addr = f"{info.get('transport', 'tcp')}://{info['ip']}:{info['hb_port']}"
    except Exception:
        return False
    ctx = zmq.Context.instance()
    s = ctx.socket(zmq.REQ)
    s.linger = 0
    try:
        s.connect(addr)
        s.send(b"ping")
        return bool(s.poll(int(timeout * 1000)))
    except Exception:
        return False
    finally:
        s.close()


def find_kernel(pattern: str = "jotter-*.json", exclude: set[str] | None = None) -> str | None:
    """Newest live kernel whose connection file matches pattern."""
    for cf in candidate_files(pattern):
        if exclude and cf in exclude:
            continue
        if kernel_alive(cf):
            return cf
    return None


@dataclass
class Cell:
    msg_id: str
    count: int | None
    code: str
    outputs: list[str] = field(default_factory=list)
    error: str | None = None
    done: bool = False
    t: float = field(default_factory=time.time)

    def render(self, code_max: int = 1500, out_max: int = 800) -> str:
        code = self.code if len(self.code) <= code_max else self.code[:code_max] + "\n# ...(truncated)"
        n = self.count if self.count is not None else "?"
        parts = [f"In [{n}]:\n{code}"]
        out = "".join(self.outputs).strip()
        if out:
            if len(out) > out_max:
                out = out[: out_max // 2] + "\n...(truncated)...\n" + out[-out_max // 2 :]
            parts.append(f"Out [{n}]:\n{out}")
        if self.error:
            parts.append(f"Error [{n}]:\n{self.error}")
        if not self.done:
            parts.append("(still running)")
        return "\n".join(parts)


PY_HELPER = r'''
def _mathbuddy_vars():
    import types
    skip = {"In", "Out", "exit", "quit", "get_ipython", "open"}
    rows = []
    for k, v in list(globals().items()):
        if k.startswith("_") or k in skip or isinstance(v, types.ModuleType):
            continue
        t = type(v).__name__
        extra = ""
        shape = getattr(v, "shape", None)
        if shape is not None and not callable(shape):
            try:
                extra = f" shape={tuple(shape)}"
            except Exception:
                pass
        elif isinstance(v, (list, tuple, dict, set, str)):
            extra = f" len={len(v)}"
        dt = getattr(v, "dtype", None)
        if dt is not None and not callable(dt):
            extra += f" dtype={dt}"
        rows.append(f"{k}: {t}{extra}")
    return "\n".join(rows[:300]) or "(no user variables)"

def _mathbuddy_describe(expr, maxlen=2500):
    v = eval(expr, globals())
    t = type(v)
    lines = [f"{expr}: {t.__module__}.{t.__qualname__}"]
    try:
        import pandas as pd
        if isinstance(v, pd.DataFrame):
            lines.append(f"shape={v.shape}")
            lines.append("dtypes:\n" + v.dtypes.to_string())
            lines.append("head:\n" + v.head(8).to_string())
            return "\n".join(lines)[:maxlen]
        if isinstance(v, pd.Series):
            lines.append(f"len={len(v)} dtype={v.dtype} name={v.name!r}")
            lines.append("head:\n" + v.head(8).to_string())
            return "\n".join(lines)[:maxlen]
    except ImportError:
        pass
    shape = getattr(v, "shape", None)
    if shape is not None and not callable(shape):
        lines.append(f"shape={tuple(shape)} dtype={getattr(v, 'dtype', '?')}")
        try:
            import numpy as np
            a = np.asarray(v)
            if a.size and a.size <= 50_000_000 and np.issubdtype(a.dtype, np.number):
                lines.append(f"min={a.min()!r} max={a.max()!r} mean={a.mean()!r}")
        except Exception:
            pass
    if callable(v):
        import inspect
        try:
            lines.append(f"signature: {expr}{inspect.signature(v)}")
        except Exception:
            pass
        doc = (getattr(v, "__doc__", None) or "").strip()
        if doc:
            lines.append("doc: " + doc[:600])
    r = repr(v)
    if len(r) > maxlen:
        r = r[: maxlen // 2] + " ... " + r[-maxlen // 4 :]
    lines.append("repr: " + r)
    return "\n".join(lines)[:maxlen]
'''


class KernelWatch:
    """One connection to one kernel: watches IOPub, runs our own requests."""

    def __init__(self, connection_file: str, on_cell_done=None):
        self.cf = connection_file
        self.on_cell_done = on_cell_done
        self.kc = jupyter_client.BlockingKernelClient(connection_file=connection_file)
        self.kc.load_connection_file()
        self.kc.start_channels()
        self.cells: OrderedDict[str, Cell] = OrderedDict()
        self.cells_lock = threading.Lock()
        self.shell_lock = threading.Lock()
        self._mine: dict[str, queue.Queue] = {}
        self._stop = threading.Event()
        self.language = "unknown"
        self.kernel_name = os.path.basename(connection_file)
        self._helper_ready = False
        self._load_info()
        self._seed_history()
        self._thread = threading.Thread(target=self._iopub_loop, daemon=True)
        self._thread.start()

    # -- shell requests (ours) ---------------------------------------------

    def _request(self, msg_type: str, content: dict, timeout: float = 30.0, collect: bool = False):
        """Send on our shell socket. Our msg_id is registered before sending so
        the IOPub thread never mistakes our traffic for the user's."""
        with self.shell_lock:
            msg = self.kc.session.msg(msg_type, content)
            mid = msg["header"]["msg_id"]
            q: queue.Queue = queue.Queue()
            self._mine[mid] = q
            try:
                self.kc.shell_channel.send(msg)
                deadline = time.time() + timeout
                while True:
                    left = deadline - time.time()
                    if left <= 0:
                        raise TimeoutError(f"kernel did not answer {msg_type} in {timeout:.0f}s (is it busy?)")
                    reply = self.kc.get_shell_msg(timeout=left)
                    if reply["parent_header"].get("msg_id") == mid:
                        break
                outputs = []
                if collect:
                    # Drain IOPub output for this request until the kernel goes idle.
                    while True:
                        try:
                            m = q.get(timeout=max(0.1, deadline - time.time()))
                        except queue.Empty:
                            break
                        if m is None:
                            break
                        outputs.append(m)
                return reply, outputs
            finally:
                self._mine.pop(mid, None)

    def _load_info(self):
        try:
            reply, _ = self._request("kernel_info_request", {}, timeout=10)
            li = reply["content"].get("language_info", {})
            self.language = li.get("name", "unknown")
            impl = reply["content"].get("implementation", "")
            self.kernel_name = f"{self.language} ({impl})" if impl else self.language
        except Exception:
            pass

    def _seed_history(self):
        """Recent inputs from before we attached, so we're not starting blind.

        Only the current session (session 0 in IPython's numbering): a "tail"
        request would also return code from every earlier session in
        ~/.ipython, which the model would mistake for what you're doing now."""
        try:
            reply, _ = self._request(
                "history_request",
                {"output": False, "raw": True, "hist_access_type": "range", "session": 0, "start": 1, "stop": None},
                timeout=5,
            )
            for session, line, src in reply["content"].get("history", [])[-12:]:
                if not src or not src.strip() or "_mathbuddy" in src:
                    continue
                mid = f"history-{session}-{line}"
                self._add_cell(Cell(mid, line, src, done=True))
        except Exception:
            pass

    @property
    def python(self) -> bool:
        return self.language == "python"

    @property
    def can_inspect(self) -> bool:
        return self.python

    def _ensure_helper(self):
        if self._helper_ready:
            return
        self._request(
            "execute_request",
            {"code": PY_HELPER, "silent": True, "store_history": False, "user_expressions": {}, "allow_stdin": False},
        )
        self._helper_ready = True

    def _user_expr(self, expr: str, timeout: float = 20.0) -> str:
        """Evaluate one expression silently: nothing appears in jotter, no
        history entry, the result comes back in the reply only."""
        for attempt in (0, 1):
            self._ensure_helper()
            reply, _ = self._request(
                "execute_request",
                {"code": "", "silent": True, "store_history": False, "user_expressions": {"r": expr}, "allow_stdin": False},
                timeout=timeout,
            )
            r = reply["content"].get("user_expressions", {}).get("r", {})
            if r.get("status") == "ok":
                txt = r.get("data", {}).get("text/plain", "")
                try:
                    return ast.literal_eval(txt)
                except Exception:
                    return txt
            if r.get("ename") == "NameError" and "_mathbuddy" in r.get("evalue", "") and attempt == 0:
                self._helper_ready = False  # kernel restarted under us; reinstall
                continue
            return f"{r.get('ename', 'Error')}: {r.get('evalue', '')}"
        return "error"

    def list_variables(self) -> str:
        if not self.python:
            return f"(variable listing is only implemented for Python kernels; this is {self.language})"
        return self._user_expr("_mathbuddy_vars()")

    def inspect(self, expr: str) -> str:
        if not self.python:
            return f"(inspection is only implemented for Python kernels; this is {self.language})"
        expr = expr.strip()
        if not INSPECTABLE.match(expr):
            return "refused: only names, attributes and simple subscripts can be inspected (no calls)"
        return self._user_expr(f"_mathbuddy_describe({expr!r})")

    def run(self, code: str, timeout: float = 300.0) -> tuple[str, str]:
        """Run code in the user's namespace on their say-so. Kept in kernel
        history (In[]) so it isn't invisible, but it is not a notebook cell."""
        reply, outputs = self._request(
            "execute_request",
            {"code": code, "silent": False, "store_history": True, "user_expressions": {}, "allow_stdin": False},
            timeout=timeout,
            collect=True,
        )
        text, plots = [], 0
        for m in outputs:
            t, c = m["msg_type"], m["content"]
            if t == "stream":
                text.append(c["text"])
            elif t in ("execute_result", "display_data"):
                d = c.get("data", {})
                if any(k.startswith("image/") for k in d):
                    plots += 1
                else:
                    text.append(d.get("text/plain", f"[{', '.join(d)}]") + "\n")
            elif t == "error":
                text.append(strip_ansi("\n".join(c.get("traceback", [])) or f"{c['ename']}: {c['evalue']}") + "\n")
        if plots:
            # The image went to mathbuddy, not to a notebook cell, so the user can't see it yet.
            text.append(f"(drew {plots} plot{'s' if plots > 1 else ''}; it isn't shown in the notebook "
                        "because mathbuddy ran the code, not a cell: /copy it into a cell to see it)\n")
        return reply["content"].get("status", "?"), "".join(text)

    # -- IOPub (everyone's) ------------------------------------------------

    def _add_cell(self, cell: Cell):
        with self.cells_lock:
            self.cells[cell.msg_id] = cell
            while len(self.cells) > MAX_CELLS:
                self.cells.popitem(last=False)

    def _iopub_loop(self):
        while not self._stop.is_set():
            try:
                msg = self.kc.get_iopub_msg(timeout=0.5)
            except queue.Empty:
                continue
            except Exception:
                if self._stop.is_set():
                    return
                time.sleep(0.5)
                continue
            pid = msg["parent_header"].get("msg_id")
            t, c = msg["msg_type"], msg["content"]
            q = self._mine.get(pid)
            if q is not None:
                if t == "status" and c.get("execution_state") == "idle":
                    q.put(None)
                elif t in ("stream", "execute_result", "display_data", "error"):
                    q.put(msg)
                continue
            if t == "status" and c.get("execution_state") == "starting":
                self._helper_ready = False
                continue
            if t == "execute_input":
                self._add_cell(Cell(pid, c.get("execution_count"), c.get("code", "")))
                continue
            cell = self.cells.get(pid)
            if cell is None:
                continue
            if t == "stream":
                cell.outputs.append(c.get("text", ""))
            elif t in ("execute_result", "display_data"):
                d = c.get("data", {})
                if "text/plain" in d:
                    cell.outputs.append(d["text/plain"] + "\n")
                others = [k for k in d if k != "text/plain"]
                if others:
                    cell.outputs.append(f"[rich output: {', '.join(others)}]\n")
            elif t == "error":
                tb = strip_ansi("\n".join(c.get("traceback", [])))
                tb_lines = tb.splitlines()
                if len(tb_lines) > 25:
                    tb = "\n".join(tb_lines[:5] + ["..."] + tb_lines[-18:])
                cell.error = tb or f"{c.get('ename')}: {c.get('evalue')}"
            elif t == "status" and c.get("execution_state") == "idle":
                cell.done = True
                if self.on_cell_done:
                    try:
                        self.on_cell_done(cell)
                    except Exception:
                        pass

    def recent(self, n: int = 8) -> list[Cell]:
        with self.cells_lock:
            return list(self.cells.values())[-n:]

    def alive(self) -> bool:
        return kernel_alive(self.cf, timeout=2)

    def close(self):
        self._stop.set()
        try:
            self.kc.stop_channels()
        except Exception:
            pass
