"""Find the sessions mathbuddy could attach to, and suggest one.

Two kinds: Jupyter kernels (jotter, euporie, Jupyter servers), found by their
connection files, and R sessions running mathbuddy's bridge (RStudio,
terminal R), found by the bridge's registry files.

The suggestion prefers, in order: the session mathbuddy was started from
inside (RStudio's Terminal tab is a child of its R session), a session in the
same tmux window, a session in the same folder, and then the newest.
"""

from __future__ import annotations

import glob
import json
import os
import subprocess
import time
from dataclasses import dataclass

from .kernel import candidate_files, kernel_alive
from .rbridge import RBridge, bridge_dir, pid_alive

JUPYTER_PATTERNS = ["jotter-*.json", "kernel-*.json"]
_dead: dict[str, float] = {}  # connection file -> mtime when found dead (they don't come back)


@dataclass
class Session:
    kind: str  # "jupyter" or "r-bridge"
    path: str
    language: str
    frontend: str
    label: str
    started: float
    pid: int | None = None  # the frontend's process (jotter, euporie) or R itself
    cwd: str = ""
    score: int = 0
    reason: str = ""

    def title(self) -> str:
        return f"{self.language} · {self.frontend} · {self.label}"

    def line(self) -> str:
        age = time.time() - self.started
        when = f"{int(age // 60)} min ago" if age < 3600 else f"{age / 3600:.1f} h ago"
        why = f"  ← {self.reason}" if self.reason else ""
        return f"{self.title()}  (started {when}){why}"

    def matches(self, word: str) -> bool:
        hay = f"{self.language} {self.frontend} {self.label} {os.path.basename(self.path)} {self.cwd}".lower()
        return all(w in hay for w in word.lower().split())

    def connect(self, on_cell_done=None):
        if self.kind == "r-bridge":
            return RBridge(self.path, on_cell_done=on_cell_done)
        from .kernel import KernelWatch

        return KernelWatch(self.path, on_cell_done=on_cell_done)


# -- /proc helpers ------------------------------------------------------------

def _cmdline(pid: int) -> list[str]:
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return [a.decode(errors="replace") for a in f.read().split(b"\0") if a]
    except OSError:
        return []


def _ppid(pid: int) -> int | None:
    try:
        with open(f"/proc/{pid}/stat") as f:
            return int(f.read().rsplit(")", 1)[1].split()[1])
    except (OSError, IndexError, ValueError):
        return None


def _cwd(pid: int) -> str:
    try:
        return os.readlink(f"/proc/{pid}/cwd")
    except OSError:
        return ""


def ancestors(pid: int) -> list[int]:
    chain = []
    while pid and pid > 1 and len(chain) < 64:
        chain.append(pid)
        pid = _ppid(pid)
    return chain


def _kernel_pids() -> dict[str, int]:
    """Connection file -> kernel pid, from the kernels' command lines."""
    out = {}
    for d in glob.glob("/proc/[0-9]*"):
        pid = int(d[6:])
        for a in _cmdline(pid):
            if a.endswith(".json") and ("jotter-" in a or "kernel-" in a):
                out[os.path.realpath(a)] = pid
    return out


def _language(cmd: list[str], cf: str) -> str:
    s = " ".join(cmd).lower()
    for key, name in (("ipykernel", "Python"), ("irkernel", "R"), ("octave", "Octave"),
                      ("wolfram", "Wolfram"), ("julia", "Julia")):
        if key in s:
            return name
    try:
        with open(cf) as f:
            kn = json.load(f).get("kernel_name", "")
        if kn:
            return kn
    except Exception:
        pass
    return "unknown"


def _frontend(pid: int | None) -> tuple[str, str, int | None]:
    """(frontend name, label, frontend pid) for the process that started a kernel."""
    if not pid:
        return "Jupyter", "?", None
    cmd = _cmdline(pid)
    exe = os.path.basename(cmd[0]) if cmd else "?"
    if exe.startswith("python") and len(cmd) > 1:
        exe = os.path.basename(cmd[1])
    nb = next((a for a in cmd[1:] if a.endswith(".ipynb")), None)
    name = {"jotter": "jotter", "euporie": "euporie", "euporie-notebook": "euporie"}.get(exe)
    if name is None:
        name = "Jupyter" if "jupyter" in " ".join(cmd) else exe
    label = os.path.basename(nb) if nb else ("scratchpad" if name == "jotter" else os.path.basename(_cwd(pid)) or "?")
    return name, label, pid


# -- discovery ------------------------------------------------------------------

def discover(patterns: list[str] | None = None, probe_r: bool = True) -> list[Session]:
    sessions: list[Session] = []
    pids = None
    for pat in patterns or JUPYTER_PATTERNS:
        for cf in candidate_files(pat):
            real = os.path.realpath(cf)
            try:
                mtime = os.path.getmtime(cf)
            except OSError:
                continue
            if _dead.get(real) == mtime:
                continue
            if not kernel_alive(cf, timeout=0.3):
                _dead[real] = mtime
                continue
            if pids is None:
                pids = _kernel_pids()
            kpid = pids.get(real)
            front, label, fpid = _frontend(_ppid(kpid) if kpid else None)
            sessions.append(Session("jupyter", cf, _language(_cmdline(kpid) if kpid else [], cf),
                                    front, label, mtime, fpid, _cwd(fpid) if fpid else ""))
    for reg in glob.glob(os.path.join(bridge_dir(), "r-*.json")):
        try:
            with open(reg) as f:
                info = json.load(f)
            pid = int(info["pid"])
        except Exception:
            continue
        if not pid_alive(pid):
            try:
                os.remove(reg)  # our own registry file, left by an R that was killed
            except OSError:
                pass
            continue
        label = os.path.basename(info.get("cwd", "")) or "R"
        if probe_r:  # the project name, if R is idle enough to say
            try:
                import urllib.request

                req = urllib.request.Request(f"http://127.0.0.1:{int(info['port'])}/info",
                                             headers={"X-Mathbuddy-Token": info["token"]})
                with urllib.request.urlopen(req, timeout=0.8) as r:
                    label = json.load(r).get("label") or label
            except Exception:
                pass
        sessions.append(Session("r-bridge", reg, "R", info.get("frontend", "R"), label,
                                float(info.get("started", os.path.getmtime(reg))), pid, info.get("cwd", "")))
    return sessions


def _tmux_window_pane_pids() -> set[int]:
    pane = os.environ.get("TMUX_PANE")
    if not os.environ.get("TMUX") or not pane:
        return set()
    try:
        win = subprocess.run(["tmux", "display-message", "-p", "-t", pane, "#{window_id}"],
                             capture_output=True, text=True, timeout=2).stdout.strip()
        out = subprocess.run(["tmux", "list-panes", "-t", win, "-F", "#{pane_pid}"],
                             capture_output=True, text=True, timeout=2).stdout.split()
        return {int(p) for p in out if p.isdigit()}
    except Exception:
        return set()


def rank(sessions: list[Session]) -> list[Session]:
    """Score and order sessions, best suggestion first."""
    mine = set(ancestors(os.getpid()))
    panes = _tmux_window_pane_pids()
    here = os.getcwd()
    for s in sessions:
        s.score, s.reason = 0, ""
        if not s.pid:
            continue
        chain = ancestors(s.pid)
        if s.pid in mine:
            s.score, s.reason = 3, f"you started mathbuddy from inside this {s.frontend}"
        elif panes and panes & set(chain):
            s.score, s.reason = 2, "next to you in this tmux window"
        elif s.cwd and s.cwd == here:
            s.score, s.reason = 1, "same folder as you"
    sessions.sort(key=lambda s: (s.score, s.started), reverse=True)
    if sessions and not sessions[0].reason:
        sessions[0].reason = "the newest"
    return sessions


def pick(sessions: list[Session], target: str | None) -> Session | None:
    """target: None (best suggestion), a 1-based number, or words to match."""
    if not sessions:
        return None
    if not target:
        return sessions[0]
    if target.isdigit():
        i = int(target) - 1
        return sessions[i] if 0 <= i < len(sessions) else None
    hits = [s for s in sessions if s.matches(target)]
    return hits[0] if hits else None
