"""Against a real ipykernel. A second client plays jotter; a third checks that
mathbuddy's own inspections stay invisible to other frontends."""

import json
import os
import queue
import time

import jupyter_client
import pytest

from mathbuddy.assistant import Assistant, code_blocks
from mathbuddy.kernel import KernelWatch, kernel_alive


@pytest.fixture(scope="module")
def kernel(tmp_path_factory):
    # Own IPython dir: keeps test inputs out of the real ~/.ipython history.
    ipy = tmp_path_factory.mktemp("ipython")
    km = jupyter_client.KernelManager(kernel_name="python3")
    km.start_kernel(env={**os.environ, "IPYTHONDIR": str(ipy)})
    jotter = km.client()
    jotter.start_channels()
    jotter.wait_for_ready(timeout=60)
    yield km, jotter
    jotter.stop_channels()
    km.shutdown_kernel(now=True)


def run_as_jotter(client, code, timeout=30):
    """Execute like a frontend does: send, wait for our reply and idle."""
    mid = client.execute(code)
    end = time.time() + timeout
    while time.time() < end:
        m = client.get_iopub_msg(timeout=end - time.time())
        if (m["parent_header"].get("msg_id") == mid and m["msg_type"] == "status"
                and m["content"]["execution_state"] == "idle"):
            return
    raise TimeoutError(code)


def wait_for(pred, timeout=15):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(0.05)
    return False


def test_alive_and_stale(kernel, tmp_path):
    km, _ = kernel
    assert kernel_alive(km.connection_file)
    stale = tmp_path / "jotter-dead.json"
    info = json.load(open(km.connection_file))
    info["hb_port"] = 1  # nothing listens there
    stale.write_text(json.dumps(info))
    assert not kernel_alive(str(stale), timeout=0.5)


def test_watch_sees_frontend_cells(kernel):
    km, jotter = kernel
    done = []
    w = KernelWatch(km.connection_file, on_cell_done=done.append)
    try:
        assert w.language == "python"
        jotter.execute("import numpy as np\nx = np.arange(12.0).reshape(3, 4)\nprint('sum', x.sum())")
        jotter.execute("x.mean()")
        jotter.execute("1/0")
        assert wait_for(lambda: len(done) >= 3)
        a, b, c = done[-3:]
        assert "sum 66.0" in "".join(a.outputs)
        assert "5.5" in "".join(b.outputs)
        assert c.error and "ZeroDivisionError" in c.error
        assert "\x1b[" not in c.error
        assert "x = np.arange" in w.recent()[-3].render()
    finally:
        w.close()


def test_inspection_is_silent_to_other_frontends(kernel):
    km, jotter = kernel
    run_as_jotter(jotter, "import pandas as pd\ndf = pd.DataFrame({'T_K': [293.0, 303.0], 'p_kPa': [101.3, 99.8]})")
    # A separate client with its own session, as another frontend process would be.
    # (km.client() would share one Session object with the "jotter" client.)
    observer = jupyter_client.BlockingKernelClient(connection_file=km.connection_file)
    observer.load_connection_file()
    observer.start_channels()
    w = KernelWatch(km.connection_file)
    try:
        time.sleep(0.3)
        while True:  # drain
            try:
                observer.get_iopub_msg(timeout=0.2)
            except queue.Empty:
                break
        vars_ = w.list_variables()
        assert "df: DataFrame shape=(2, 2)" in vars_
        d = w.inspect("df")
        assert "T_K" in d and "float64" in d and "293.0" in d
        assert "p_kPa" in w.inspect("df['p_kPa']")
        assert w.inspect("df.drop(columns='T_K')").startswith("refused")
        seen = []
        while True:
            try:
                seen.append(observer.get_iopub_msg(timeout=0.5)["msg_type"])
            except queue.Empty:
                break
        assert "execute_input" not in seen and "execute_result" not in seen and "stream" not in seen
        assert not any("mathbuddy" in c.code for c in w.recent(100))
    finally:
        w.close()
        observer.stop_channels()


def test_run_lands_in_namespace(kernel):
    km, _ = kernel
    w = KernelWatch(km.connection_file)
    try:
        status, out = w.run("y = 6 * 7\nprint(y)")
        assert status == "ok" and out.strip() == "42"
        assert "y: int" in w.list_variables()
        status, out = w.run("undefined_name")
        assert status == "error" and "NameError" in out
        assert not any(c.code.startswith("y = 6") for c in w.recent(100))  # ours, not the user's cell
    finally:
        w.close()


def test_history_seed_is_current_session_only(kernel):
    km, jotter = kernel
    run_as_jotter(jotter, "seed_marker = 1")
    w = KernelWatch(km.connection_file)
    try:
        codes = [c.code for c in w.recent(100)]
        assert "seed_marker = 1" in codes
        assert len(codes) <= 12
    finally:
        w.close()


def test_helper_reinstalled_after_restart(kernel):
    km, jotter = kernel
    w = KernelWatch(km.connection_file)
    try:
        w.list_variables()
        run_as_jotter(jotter, "del _mathbuddy_vars")  # same effect as a restart
        assert "(no user variables)" in w.list_variables() or ":" in w.list_variables()
    finally:
        w.close()


class FakeLLM:
    """Asks for one inspection, then answers using what it saw."""

    def __init__(self):
        self.seen = []

    def chat(self, messages, tools=None, **kw):
        self.seen.append(messages)
        if messages[-1]["role"] == "user":
            return {"content": "", "tool_calls": [{"function": {"name": "inspect", "arguments": {"expr": "df"}}}]}
        tool_out = messages[-1]["content"]
        return {"content": f"Columns seen: {'T_K' in tool_out}\n```python\ndf['T_C'] = df['T_K'] - 273.15\n```",
                "tool_calls": []}


def test_assistant_tool_loop(kernel):
    km, jotter = kernel
    w = KernelWatch(km.connection_file)
    try:
        run_as_jotter(jotter, "z = df['T_K'].max()")
        assert wait_for(lambda: any("z = df" in c.code for c in w.recent()))
        llm = FakeLLM()
        a = Assistant(llm)
        ans = a.ask(w, "add a Celsius column")
        assert "Columns seen: True" in ans
        assert code_blocks(ans) == ["df['T_C'] = df['T_K'] - 273.15"]
        system = llm.seen[0][0]["content"]
        assert "z = df['T_K'].max()" in system  # the model sees the live session
    finally:
        w.close()


class ScriptedLLM:
    """Plays back a fixed list of tool calls, then reports the last tool output."""

    def __init__(self, calls):
        self.calls = list(calls)
        self.tools_offered = []

    def chat(self, messages, tools=None, **kw):
        self.tools_offered.append(tools is not None)
        if self.calls and tools is not None:
            name, args = self.calls.pop(0)
            return {"content": "", "tool_calls": [{"function": {"name": name, "arguments": args}}]}
        last_tool = [m for m in messages if m["role"] == "tool"][-1]["content"]
        return {"content": "TOOL SAID: " + last_tool, "tool_calls": []}


def test_invented_tool_is_explained(kernel):
    km, _ = kernel
    w = KernelWatch(km.connection_file)
    try:
        ans = Assistant(ScriptedLLM([("run", {"code": "1"})])).ask(w, "x")
        assert "no tool named 'run'" in ans and "run_code" in ans
    finally:
        w.close()


def test_run_code_needs_approval(kernel):
    km, _ = kernel
    w = KernelWatch(km.connection_file)
    try:
        asked = []
        a = Assistant(ScriptedLLM([("run_code", {"code": "declined_var = 1"})]),
                      confirm=lambda c: asked.append(c) or False)
        assert "declined" in a.ask(w, "x")
        assert asked == ["declined_var = 1"]
        assert "declined_var" not in w.list_variables()

        a = Assistant(ScriptedLLM([("run_code", {"code": "import math\nprint(round(math.pi, 4))"})]),
                      confirm=lambda c: True)
        assert "3.1416" in a.ask(w, "x")
    finally:
        w.close()


def test_tool_rounds_are_capped(kernel):
    km, _ = kernel
    w = KernelWatch(km.connection_file)
    try:
        llm = ScriptedLLM([("list_variables", {})] * 50)
        a = Assistant(llm, max_rounds=3)
        a.ask(w, "x")
        assert llm.tools_offered == [True, True, True, False]  # last round forced to answer
    finally:
        w.close()


def test_python_plot_is_reported(kernel):
    pytest.importorskip("matplotlib")
    km, _ = kernel
    w = KernelWatch(km.connection_file)
    try:
        status, out = w.run("import matplotlib\nmatplotlib.use('module://matplotlib_inline.backend_inline')\n"
                            "import matplotlib.pyplot as plt\nplt.plot([1, 2, 3]); plt.show()")
        assert status == "ok", out
        assert "drew 1 plot" in out and "/copy it into a cell" in out
    finally:
        w.close()
