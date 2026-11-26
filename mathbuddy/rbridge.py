"""Connector for an R session running mathbuddy's bridge (RStudio, terminal R).

The bridge (r/bridge.R) runs inside the R session and answers on localhost
while R is idle. Same surface as KernelWatch, so the assistant can't tell the
difference.
"""

from __future__ import annotations

import json
import os
import re
import threading
import urllib.error
import urllib.request

from .kernel import Cell

BRIDGE_R = os.path.join(os.path.dirname(__file__), "r", "bridge.R")
# Pre-check before the bridge's own (stricter, parse-based) check.
R_INSPECTABLE = re.compile(r"^[A-Za-z.][\w.]*(\$[A-Za-z.][\w.]*|\[\[?[^\[\]()]{1,40}\]?\]|@[A-Za-z.][\w.]*)*$")


def bridge_dir() -> str:
    d = os.environ.get("XDG_RUNTIME_DIR") or ""
    if not d or not os.path.isdir(d):
        import tempfile

        d = tempfile.gettempdir()
    return os.path.join(d, "mathbuddy")


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


class BridgeError(RuntimeError):
    pass


class RBridge:
    can_inspect = True
    python = False
    language = "R"

    def __init__(self, registry_file: str, on_cell_done=None, poll: float = 1.0):
        self.cf = registry_file
        with open(registry_file) as f:
            reg = json.load(f)
        self.pid = int(reg["pid"])
        self.base = f"http://127.0.0.1:{int(reg['port'])}"
        self.token = reg["token"]
        self.frontend = reg.get("frontend", "R")
        self.on_cell_done = on_cell_done
        self.cells: list[Cell] = []
        self.lock = threading.Lock()
        self.seq = 0
        self.info: dict = {}
        self._stop = threading.Event()
        self.refresh_info()
        self.kernel_name = f"R {self.info.get('r_version', '')} ({self.frontend})".replace("R  (", "R (")
        self._poll_events(notify=False)  # what you did before mathbuddy arrived
        self._thread = threading.Thread(target=self._loop, args=(poll,), daemon=True)
        self._thread.start()

    # -- transport --------------------------------------------------------------

    def _call(self, path: str, body: dict | None = None, timeout: float = 20.0) -> dict:
        req = urllib.request.Request(
            self.base + path,
            data=json.dumps(body).encode() if body is not None else None,
            headers={"X-Mathbuddy-Token": self.token, "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                out = json.load(r)
        except TimeoutError:
            raise BridgeError(f"R did not answer in {timeout:.0f}s (is it busy running something?)") from None
        except urllib.error.HTTPError as e:
            raise BridgeError(f"bridge: HTTP {e.code} {e.read().decode(errors='replace')[:200]}") from None
        except urllib.error.URLError as e:
            if isinstance(e.reason, TimeoutError):
                raise BridgeError(f"R did not answer in {timeout:.0f}s (is it busy running something?)") from None
            raise BridgeError(f"bridge unreachable ({e.reason})") from None
        if isinstance(out, dict) and out.get("error"):
            raise BridgeError(out["error"])
        return out

    def refresh_info(self) -> dict:
        try:
            self.info = self._call("/info", timeout=5)
        except BridgeError:
            pass
        return self.info

    # -- watching -------------------------------------------------------------

    def _poll_events(self, notify: bool = True):
        out = self._call(f"/events?since={self.seq}", timeout=5)
        for ev in out.get("events", []):
            cell = Cell(f"r-{ev['seq']}", ev["seq"], ev.get("code", ""), done=True)
            if ev.get("value"):
                cell.outputs.append(ev["value"])
            cell.error = ev.get("error")
            with self.lock:
                self.cells.append(cell)
                self.cells = self.cells[-60:]
            self.seq = max(self.seq, int(ev["seq"]))
            if notify and self.on_cell_done:
                try:
                    self.on_cell_done(cell)
                except Exception:
                    pass

    def _loop(self, poll: float):
        while not self._stop.wait(poll):
            try:
                self._poll_events()
            except BridgeError:
                pass  # R is busy; try again next tick

    def recent(self, n: int = 8) -> list[Cell]:
        with self.lock:
            return self.cells[-n:]

    # -- tools ------------------------------------------------------------------

    def list_variables(self) -> str:
        return self._call("/vars")["text"]

    def inspect(self, expr: str) -> str:
        expr = expr.strip()
        if not R_INSPECTABLE.match(expr):
            return "refused: only names, $, [[ ]] and [ ] with constants can be inspected (no function calls)"
        return self._call("/inspect", {"expr": expr})["text"]

    def run(self, code: str, timeout: float = 300.0) -> tuple[str, str]:
        out = self._call("/run", {"code": code}, timeout=timeout)
        text = out.get("output", "")
        n = int(out.get("plots") or 0)
        if n:
            dev = out.get("device") or "the active device"
            where = "the RStudio Plots pane" if dev == "RStudioGD" else dev
            text = (text.rstrip() + "\n" if text.strip() else "") + f"(drew {n} plot{'s' if n > 1 else ''} in {where})"
        return out.get("status", "?"), text

    def _editor(self, path: str, code: str) -> str:
        out = self._call(path, {"code": code}, timeout=10)
        if out.get("status") != "ok":
            raise BridgeError(out.get("output", "failed"))
        return out.get("output", "")

    def insert(self, code: str) -> str:
        return self._editor("/insert", code)

    def to_console(self, code: str) -> str:
        return self._editor("/console", code)

    def alive(self) -> bool:
        return pid_alive(self.pid) and os.path.exists(self.cf)

    def close(self):
        self._stop.set()
