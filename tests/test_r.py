"""R bridge, session discovery and ranking. A real interactive R process,
fed through a pipe, plays the user's console."""

import json
import os
import shutil
import subprocess
import time

import pytest

from mathbuddy.assistant import Assistant
from mathbuddy.rbridge import BRIDGE_R, RBridge
from mathbuddy.sessions import Session, discover, pick, rank

pytestmark = pytest.mark.skipif(shutil.which("R") is None, reason="R not installed")


def wait_for(pred, timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(0.1)
    return False


@pytest.fixture(scope="module")
def rsession(tmp_path_factory):
    run = tmp_path_factory.mktemp("xdg")
    work = tmp_path_factory.mktemp("rproject")
    env = {**os.environ, "XDG_RUNTIME_DIR": str(run)}
    env.pop("RSTUDIO", None)
    p = subprocess.Popen(["R", "--interactive", "--no-save", "--quiet", "--no-readline"],
                         stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         cwd=work, env=env, text=True)

    def type_(line):
        p.stdin.write(line + "\n")
        p.stdin.flush()

    type_(f'source("{BRIDGE_R}")')
    reg = run / "mathbuddy" / f"r-{p.pid}.json"
    assert wait_for(reg.exists), "bridge did not register"
    old = os.environ.get("XDG_RUNTIME_DIR")
    os.environ["XDG_RUNTIME_DIR"] = str(run)
    yield p, type_, reg, work
    if old is None:
        del os.environ["XDG_RUNTIME_DIR"]
    else:
        os.environ["XDG_RUNTIME_DIR"] = old
    type_("q()")
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
    assert not reg.exists(), "registry file should be removed when R exits"


def test_registry_is_private(rsession):
    _, _, reg, _ = rsession
    assert oct(os.stat(reg).st_mode & 0o777) == "0o600"
    assert {"pid", "port", "token"} <= set(json.load(open(reg)))


def test_discovered_and_labelled(rsession):
    p, _, reg, work = rsession
    found = [s for s in discover(probe_r=True) if s.kind == "r-bridge"]
    assert len(found) == 1
    s = found[0]
    assert s.pid == p.pid and s.language == "R" and s.frontend == "R console"
    assert s.label == os.path.basename(work)


def test_sees_commands_and_errors(rsession):
    _, type_, reg, _ = rsession
    done = []
    b = RBridge(str(reg), on_cell_done=done.append, poll=0.2)
    try:
        type_('AAA <- c(4.1, 3.9, 4.4); BBB <- c(5.2, 5.6, 4.9)')
        type_('mean(AAA)')
        type_('log("a")')
        type_('try(log("b"), silent = TRUE)')
        assert wait_for(lambda: any(c.code.startswith("try(") for c in done))
        codes = {c.code: c for c in done}
        assert "[1] 4.133333" in "".join(codes["mean(AAA)"].outputs)
        assert "non-numeric argument" in codes['log("a")'].error
        assert codes['try(log("b"), silent = TRUE)'].error is None  # caught, so not the user's error
    finally:
        b.close()


def test_inspect_vars_and_refusal(rsession):
    _, type_, reg, _ = rsession
    type_('df <- data.frame(g = rep(c("AAA", "BBB"), each = 3), y = c(4.1, 3.9, 4.4, 5.2, 5.6, 4.9))')
    b = RBridge(str(reg))
    try:
        assert wait_for(lambda: "df: data.frame dim=6x2 cols=g,y" in b.list_variables())
        d = b.inspect("df")
        assert "head:" in d and "AAA" in d and "summary:" in d
        assert "num" in b.inspect("df$y")
        assert b.inspect("rm(df)").startswith("refused")
        assert b.inspect("df[ , 1]").startswith("refused") or "AAA" in b.inspect("df[ , 1]")
        assert "df" in b.list_variables()  # still there
    finally:
        b.close()


def test_run_plots_to_active_device(rsession, tmp_path):
    _, _, reg, _ = rsession
    b = RBridge(str(reg))
    pdf = tmp_path / "plot.pdf"
    try:
        status, out = b.run(f'pdf("{pdf}")\n'
                            "m <- tapply(df$y, df$g, mean); s <- tapply(df$y, df$g, sd)\n"
                            "b <- barplot(m, ylim = c(0, max(m + s) * 1.2))\n"
                            "arrows(b, m - s, b, m + s, angle = 90, code = 3, length = 0.1)\n"
                            "print(round(m, 3)); warning('just checking')\n"
                            "invisible(dev.off())")
        assert status == "ok", out
        assert "4.133" in out and "5.233" in out and "Warning: just checking" in out
        assert "(drew 1 plot in pdf)" in out
        assert pdf.stat().st_size > 1000
        status, out = b.run("stop('nope')")
        assert status == "error" and "nope" in out
    finally:
        b.close()


def test_outside_rstudio_insert_says_so(rsession):
    _, _, reg, _ = rsession
    b = RBridge(str(reg))
    try:
        with pytest.raises(Exception, match="not running in RStudio"):
            b.insert("1 + 1")
    finally:
        b.close()


def test_bad_token_refused(rsession):
    _, _, reg, _ = rsession
    b = RBridge(str(reg))
    b.token = "wrong"
    with pytest.raises(Exception, match="403"):
        b.list_variables()


def test_assistant_on_r(rsession):
    _, _, reg, _ = rsession
    b = RBridge(str(reg))

    class LLM:
        def __init__(self):
            self.n = 0
            self.system = ""

        def chat(self, messages, tools=None, **kw):
            self.system = messages[0]["content"]
            self.n += 1
            if self.n == 1:
                return {"content": "", "tool_calls": [{"function": {"name": "run_code",
                        "arguments": {"code": "print(round(mean(BBB) - mean(AAA), 3))"}}}]}
            return {"content": "difference: " + messages[-1]["content"], "tool_calls": []}

    try:
        llm = LLM()
        ans = Assistant(llm, confirm=lambda c: True).ask(b, "how far apart are they?")
        assert "1.1" in ans
        assert "R specifics" in llm.system and "ggplot2" in llm.system and "mean(AAA)" in llm.system
    finally:
        b.close()


def test_stale_registry_removed(rsession, tmp_path):
    _, _, reg, _ = rsession
    stale = reg.parent / "r-999999.json"
    stale.write_text(json.dumps({"pid": 999999, "port": 1, "token": "x"}))
    discover(probe_r=False)
    assert not stale.exists() and reg.exists()


def test_rank_and_pick():
    now = time.time()
    a = Session("jupyter", "/x/jotter-a.json", "Python", "jotter", "demo.ipynb", now - 10, pid=None)
    b = Session("r-bridge", "/x/r-1.json", "R", "RStudio", "assay-project", now - 500, pid=os.getppid())
    c = Session("jupyter", "/x/jotter-c.json", "Python", "jotter", "newest.ipynb", now, pid=None)
    ranked = rank([a, b, c])
    assert ranked[0] is b and "from inside this RStudio" in b.reason  # our parent beats the newest
    assert ranked[1] is c
    assert pick(ranked, None) is b
    assert pick(ranked, "2") is c
    assert pick(ranked, "demo") is a
    assert pick(ranked, "rstudio assay") is b
    assert pick(ranked, "nothing like this") is None


def test_model_fallback_choice():
    from mathbuddy.app import PREFERRED_MODELS, find_model

    laptop = ["qwen3.5:2b-q4_K_M", "qwen3.5:4b-q4_K_M", "gpt-oss:20b", "qwen3:4b"]
    desktop = ["qwen3.8-27b:latest", "qwen3-embedding:0.6b", "gemma4:latest", "qwen3.5:latest"]
    first = lambda names: next(f for m in PREFERRED_MODELS if (f := find_model(names, m)))
    assert first(desktop) == "qwen3.5:latest"
    assert first(laptop) == "qwen3.5:4b-q4_K_M"  # never the 2b, even though it is listed first
    assert first(["qwen3:4b", "gpt-oss:20b"]) == "qwen3:4b"
    assert find_model(["qwen3-embedding:0.6b"], "qwen3") is None
    assert find_model(["gemma4:latest"], "gemma4") == "gemma4:latest"


def test_user_plot_is_noticed(rsession, tmp_path):
    _, type_, reg, _ = rsession
    done = []
    b = RBridge(str(reg), on_cell_done=done.append, poll=0.2)
    try:
        type_(f'pdf("{tmp_path / "u.pdf"}"); plot(1:3); invisible(dev.off())')
        assert wait_for(lambda: any("plot(1:3)" in c.code for c in done))
        cell = next(c for c in done if "plot(1:3)" in c.code)
        assert "(drew 1 plot)" in "".join(cell.outputs)
    finally:
        b.close()


def test_runs_are_remembered(rsession):
    _, _, reg, _ = rsession
    b = RBridge(str(reg))

    class LLM:
        n = 0

        def chat(self, messages, tools=None, **kw):
            self.n += 1
            if self.n == 1:
                return {"content": "", "tool_calls": [{"function": {"name": "run_code", "arguments": {"code": "zz <- 2 + 2"}}}]}
            if self.n == 2:
                return {"content": "", "tool_calls": [{"function": {"name": "run_code", "arguments": {"code": "stop('x')"}}}]}
            return {"content": "Done: zz is 4.", "tool_calls": []}

    try:
        a = Assistant(LLM(), confirm=lambda c: True)
        a.ask(b, "make zz")
        assert a.runs == ["zz <- 2 + 2"]  # the failed run is not kept as a block
        system = a._system(b)["content"]
        assert "do NOT repeat" in system and 'geom_col(fill = "red")' in system and 'NEVER write aes(fill = "red")' in system
        assert b.info.get("rstudio_api") is False  # terminal R: the footer must not offer /insert
    finally:
        b.close()


def test_doctor_reports_fixes(tmp_path, monkeypatch, capsys):
    from mathbuddy.app import doctor

    prof = tmp_path / "Rprofile"
    prof.write_text('if (interactive()) try(source("/nowhere/bridge.R"), silent = TRUE)  # mathbuddy bridge\n')
    monkeypatch.setenv("R_PROFILE_USER", str(prof))
    monkeypatch.setenv("MATHBUDDY_OLLAMA_HOST", "127.0.0.1:9")  # nothing listens there
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    assert doctor([]) == 1
    out = capsys.readouterr().out
    assert "cannot reach Ollama" in out and "mathbuddy --host" in out
    assert "/nowhere/bridge.R, which doesn't exist" in out and "mathbuddy setup-r" in out
    assert "2 things to fix" in out
