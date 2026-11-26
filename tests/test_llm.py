"""Model clients and the model selector, against fake Ollama and llama-server
processes in this test (no real server, no model, no GPU)."""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace

import pytest

from mathbuddy.app import App
from mathbuddy.assistant import Assistant
from mathbuddy.llm import LlamaCpp, Ollama, openai_messages


class Fake:
    """A tiny HTTP server: GET paths return fixed JSON, POST /v1/chat/completions
    streams the next scripted reply as server-sent events, like llama-server."""

    def __init__(self, gets: dict, replies: list[list[dict]] | None = None):
        self.gets, self.replies, self.posts = gets, list(replies or []), []
        fake = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                if self.path not in fake.gets:
                    self.send_response(404)
                    self.end_headers()
                    return
                body = json.dumps(fake.gets[self.path]).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                fake.posts.append((self.path, json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for chunk in fake.replies.pop(0):
                    self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
                self.wfile.write(b"data: [DONE]\n\n")

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.host = f"127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()


def delta(**d):
    return {"choices": [{"index": 0, "delta": d}]}


ROUTER_MODELS = {"/v1/models": {"object": "list", "data": [
    {"id": "bonsai25", "aliases": ["bonsai25-128k"], "status": {"value": "loaded"}},
    {"id": "qwen38", "aliases": [], "status": {"value": "unloaded"}},
]}}


@pytest.fixture
def router():
    f = Fake(dict(ROUTER_MODELS))
    yield f
    f.close()


def test_router_catalog_and_aliases(router):
    lc = LlamaCpp(router.host + "/v1")  # a /v1 suffix is accepted and dropped
    assert lc.base == "http://" + router.host
    assert lc.catalog() == [("bonsai25", True), ("qwen38", False)]
    assert lc.find("bonsai25-128k") == "bonsai25" and lc.find("qwen38") == "qwen38" and lc.find("nope") is None


def test_single_model_server_counts_as_loaded():
    f = Fake({"/v1/models": {"data": [{"id": "model.gguf", "object": "model"}]}})
    try:
        assert LlamaCpp(f.host).catalog() == [("model.gguf", True)]
    finally:
        f.close()


def test_streamed_tool_call_split_across_chunks(router):
    router.replies.append([
        delta(reasoning_content="let me "), delta(reasoning_content="look"),
        delta(tool_calls=[{"index": 0, "id": "c1", "type": "function", "function": {"name": "run_code"}}]),
        delta(tool_calls=[{"index": 0, "function": {"arguments": '{"code": "import num'}}]),
        delta(tool_calls=[{"index": 0, "function": {"arguments": 'py as np; np.mean(xs)"}'}}]),
    ])
    lc = LlamaCpp(router.host, "bonsai25")
    thought = []
    r = lc.chat([{"role": "user", "content": "mean?"}], tools=[{"type": "function"}],
                on_thinking=thought.append, max_tokens=50)
    assert "".join(thought) == "let me look"
    assert r == {"content": "", "tool_calls": [
        {"id": "c1", "function": {"name": "run_code", "arguments": {"code": "import numpy as np; np.mean(xs)"}}}]}
    path, body = router.posts[0]
    assert path == "/v1/chat/completions" and body["model"] == "bonsai25" and body["max_tokens"] == 50
    assert body["chat_template_kwargs"] == {"enable_thinking": False}
    assert "temperature" not in body  # the server's preset decides sampling


def test_tool_results_are_tied_to_their_calls():
    msgs = openai_messages([
        {"role": "system", "content": "s"},
        {"role": "assistant", "content": "", "tool_calls": [
            {"id": "a", "function": {"name": "inspect", "arguments": {"expr": "df"}}},
            {"function": {"name": "run_code", "arguments": {"code": "1"}}}]},
        {"role": "tool", "tool_name": "inspect", "content": "df: 6 rows"},
        {"role": "tool", "tool_name": "run_code", "content": "1"},
    ])
    calls = msgs[1]["tool_calls"]
    assert calls[0]["id"] == "a" and json.loads(calls[0]["function"]["arguments"]) == {"expr": "df"}
    assert calls[1]["id"] and calls[1]["type"] == "function"
    assert [m["tool_call_id"] for m in msgs[2:]] == ["a", calls[1]["id"]]
    assert "tool_name" not in msgs[2]


class Watch:
    """Just enough of a session for the assistant's tool loop."""
    can_inspect = True
    language = "python"
    info: dict = {}

    def recent(self, n):
        return []

    def list_variables(self):
        return "xs: list of 5 floats"

    def run(self, code):
        return "ok", "4.72"


def test_assistant_round_trip_on_llamacpp(router, monkeypatch):
    router.replies += [
        [delta(tool_calls=[{"index": 0, "id": "k", "function": {"name": "run_code", "arguments": '{"code": "sum(xs)/len(xs)"}'}}])],
        [delta(content="The mean "), delta(content="is 4.72.")],
    ]
    a = Assistant(LlamaCpp(router.host, "bonsai25"), confirm=lambda c: True)
    monkeypatch.setattr(a, "_system", lambda watch: {"role": "system", "content": "sys"})
    assert a.ask(Watch(), "mean of xs?") == "The mean is 4.72."
    second = router.posts[1][1]["messages"]
    assert second[-2]["tool_calls"][0]["id"] == "k"
    assert second[-1] == {"role": "tool", "tool_call_id": "k", "content": second[-1]["content"]}
    assert "4.72" in second[-1]["content"]


def app_with(router, ollama_host, model=None):
    args = SimpleNamespace(model=model, host=ollama_host, cpu=False, threads=None, think=False, ctx=8192,
                           llamacpp=router.host, yes=False, no_hints=True, kernel=None)
    app = App(args)
    out = []
    app.out = out.append
    return app, out


@pytest.fixture
def ollama():
    f = Fake({"/api/tags": {"models": [{"name": "qwen3.5:latest"}, {"name": "gpt-oss:20b"}]},
              "/api/ps": {"models": [{"name": "qwen3.5:latest"}]}})
    yield f
    f.close()


def test_selector_lists_both_servers_and_switches(router, ollama):
    app, out = app_with(router, ollama.host)
    app.choose_model()  # no -m: Ollama's best, never a llama.cpp model
    assert app.llm.model == "qwen3.5:latest" and app.llm.server.kind == "ollama"
    app.command("/model")
    text = "\n".join(out)
    assert "*[1] qwen3.5:latest   (loaded)" in text and "[2] gpt-oss:20b" in text
    assert "[3] bonsai25   (loaded)" in text and "[4] qwen38" in text and "llama.cpp at" in text
    app.command("/model 3")  # loaded already: no question
    assert app.llm.label() == "bonsai25 (llama.cpp)" and app.llm.base == "http://" + router.host
    app.command("/model gpt-oss:20b")
    assert app.llm.model == "gpt-oss:20b" and app.llm.server.kind == "ollama"
    app.command("/model nope")
    assert "no model 'nope'" in out[-1]


def test_switching_to_an_unloaded_router_model_asks(router, ollama, monkeypatch):
    app, out = app_with(router, ollama.host)
    app.choose_model()
    app.interactive_mode = True
    answers = iter(["n", "y"])
    monkeypatch.setattr("prompt_toolkit.prompt", lambda *a, **k: next(answers))
    app.command("/model qwen38")
    assert "may unload bonsai25" in "\n".join(out) and app.llm.model == "qwen3.5:latest"
    app.command("/model qwen38")
    assert app.llm.model == "qwen38" and app.llm.server.kind == "llama.cpp"


def test_dash_m_finds_router_alias_and_warns(router, ollama):
    app, out = app_with(router, ollama.host, model="bonsai25-128k")
    app.choose_model()
    assert app.llm.model == "bonsai25" and app.llm.server.kind == "llama.cpp"
    app2, out2 = app_with(router, ollama.host, model="qwen38")
    app2.choose_model()
    assert app2.llm.model == "qwen38" and "may unload bonsai25" in "\n".join(out2)


def test_unreachable_llamacpp_is_reported_not_fatal(ollama):
    dead = SimpleNamespace(host="127.0.0.1:9")  # nothing listens on the discard port
    app, out = app_with(dead, ollama.host)
    app.choose_model()
    app.command("/model")
    text = "\n".join(out)
    assert "qwen3.5:latest" in text and "not reachable" in text
