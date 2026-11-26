"""Model clients: Ollama's /api/chat and llama.cpp's llama-server (OpenAI-style
/v1/chat/completions, including its router mode). Streaming and tool calls.

Plain urllib on purpose, so mathbuddy needs nothing beyond what a Jupyter
install already has.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request


def _base(host: str) -> str:
    if "://" not in host:
        host = "http://" + host
    if host.startswith("http://0.0.0.0"):
        host = host.replace("0.0.0.0", "127.0.0.1", 1)
    return host.rstrip("/")


def ollama_base(host: str | None = None) -> str:
    return _base(host or os.environ.get("OLLAMA_HOST") or "127.0.0.1:11434")


def _get_json(url: str, timeout: float = 5):
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.load(r)


def _open(name: str, base: str, url: str, body: dict):
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    try:
        return urllib.request.urlopen(req, timeout=600)
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{name}: HTTP {e.code}: {e.read().decode(errors='replace')[:300]}") from None
    except urllib.error.URLError as e:
        raise RuntimeError(f"{name}: cannot reach {base} ({e.reason})") from None


class Ollama:
    kind = "ollama"

    def __init__(self, model: str, host: str | None = None, cpu: bool = False,
                 threads: int | None = None, think: bool = False, num_ctx: int = 8192):
        self.base = ollama_base(host)
        self.model = model
        self.cpu = cpu
        self.threads = threads
        self.think = think
        self.num_ctx = num_ctx

    def options(self, extra: dict | None = None) -> dict:
        o = {"num_ctx": self.num_ctx, "temperature": 0.3}
        if self.cpu:
            o["num_gpu"] = 0  # keep every layer off the graphics card
        if self.threads:
            o["num_thread"] = self.threads
        if extra:
            o.update(extra)
        return o

    def models(self) -> list[str]:
        return [m["name"] for m in _get_json(self.base + "/api/tags").get("models", [])]

    def catalog(self) -> list[tuple[str, bool]]:
        """(name, loaded) for every model on this server. Listing loads nothing."""
        names = self.models()
        try:
            loaded = {m["name"] for m in _get_json(self.base + "/api/ps").get("models", [])}
        except Exception:
            loaded = set()
        return [(n, n in loaded) for n in names]

    def chat(self, messages: list[dict], tools: list[dict] | None = None,
             on_token=None, on_thinking=None, max_tokens: int | None = None,
             cancel=None) -> dict:
        """Stream one assistant turn. Returns {"content", "tool_calls"}."""
        body = {
            "model": self.model,
            "messages": messages,
            "stream": True,
            "think": self.think,
            "keep_alive": "15m",
            "options": self.options({"num_predict": max_tokens} if max_tokens else None),
        }
        if tools:
            body["tools"] = tools
        content, calls = [], []
        with _open("ollama", self.base, self.base + "/api/chat", body) as resp:
            for line in resp:
                if cancel is not None and cancel.is_set():
                    break
                if not line.strip():
                    continue
                chunk = json.loads(line)
                if "error" in chunk:
                    raise RuntimeError("ollama: " + chunk["error"])
                msg = chunk.get("message", {})
                if msg.get("thinking") and on_thinking:
                    on_thinking(msg["thinking"])
                if msg.get("content"):
                    content.append(msg["content"])
                    if on_token:
                        on_token(msg["content"])
                calls += msg.get("tool_calls") or []
                if chunk.get("done"):
                    break
        return {"content": "".join(content), "tool_calls": calls}


def openai_messages(messages: list[dict]) -> list[dict]:
    """mathbuddy's messages (Ollama style) in OpenAI form: tool-call arguments
    as JSON text, every call with an id, and each tool result tied to its call."""
    out, pending, n = [], [], 0
    for m in messages:
        if m.get("role") == "assistant" and m.get("tool_calls"):
            calls = []
            for c in m["tool_calls"]:
                fn = c.get("function", {})
                args = fn.get("arguments")
                if not isinstance(args, str):
                    args = json.dumps(args or {})
                n += 1
                cid = c.get("id") or f"call_{n}"
                pending.append(cid)
                calls.append({"id": cid, "type": "function",
                              "function": {"name": fn.get("name", ""), "arguments": args}})
            out.append({"role": "assistant", "content": m.get("content") or "", "tool_calls": calls})
        elif m.get("role") == "tool":
            n += 1
            out.append({"role": "tool", "tool_call_id": pending.pop(0) if pending else f"call_{n}",
                        "content": m.get("content", "")})
        else:
            out.append({k: v for k, v in m.items() if k in ("role", "content")})
    return out


class LlamaCpp:
    """llama.cpp's llama-server: one model, or a router serving several presets.

    Sampling is left to the server (its presets carry each model's card
    settings), and --cpu, --threads and --ctx don't apply: the server decides."""

    kind = "llama.cpp"
    cpu = False

    def __init__(self, host: str, model: str = "", think: bool = False):
        base = _base(host)
        self.base = base[:-3] if base.endswith("/v1") else base
        self.model = model
        self.think = think

    def _list(self) -> list[dict]:
        return _get_json(self.base + "/v1/models").get("data", [])

    def catalog(self) -> list[tuple[str, bool]]:
        """(name, loaded) for every model. A plain llama-server has one model,
        always loaded; a router lists its presets with their status."""
        out = []
        for m in self._list():
            status = m.get("status")
            state = status.get("value") if isinstance(status, dict) else None
            out.append((m["id"], state is None or state in ("loaded", "loading", "sleeping")))
        return out

    def models(self) -> list[str]:
        return [n for n, _ in self.catalog()]

    def find(self, want: str) -> str | None:
        """The server's id for `want`: an id or one of a preset's aliases."""
        for m in self._list():
            if want == m["id"] or want in (m.get("aliases") or []):
                return m["id"]
        return None

    def chat(self, messages: list[dict], tools: list[dict] | None = None,
             on_token=None, on_thinking=None, max_tokens: int | None = None,
             cancel=None) -> dict:
        """Stream one assistant turn. Returns {"content", "tool_calls"}."""
        body = {
            "model": self.model,
            "messages": openai_messages(messages),
            "stream": True,
            "chat_template_kwargs": {"enable_thinking": self.think},
        }
        if max_tokens:
            body["max_tokens"] = max_tokens
        if tools:
            body["tools"] = tools
        content, calls = [], {}
        with _open("llama.cpp", self.base, self.base + "/v1/chat/completions", body) as resp:
            for raw in resp:
                if cancel is not None and cancel.is_set():
                    break
                line = raw.decode(errors="replace").strip()
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if data == "[DONE]":
                    break
                chunk = json.loads(data)
                if "error" in chunk:
                    err = chunk["error"]
                    raise RuntimeError("llama.cpp: " + (err.get("message", str(err)) if isinstance(err, dict) else str(err)))
                for choice in chunk.get("choices") or []:
                    delta = choice.get("delta") or {}
                    if delta.get("reasoning_content") and on_thinking:
                        on_thinking(delta["reasoning_content"])
                    if delta.get("content"):
                        content.append(delta["content"])
                        if on_token:
                            on_token(delta["content"])
                    for tc in delta.get("tool_calls") or []:
                        c = calls.setdefault(tc.get("index", len(calls)), {"id": "", "name": "", "arguments": ""})
                        c["id"] = tc.get("id") or c["id"]
                        fn = tc.get("function") or {}
                        c["name"] += fn.get("name") or ""
                        c["arguments"] += fn.get("arguments") or ""
        tool_calls = []
        for _, c in sorted(calls.items()):
            try:
                args = json.loads(c["arguments"]) if c["arguments"].strip() else {}
            except ValueError:
                args = c["arguments"]  # left as text; the assistant reads it as no arguments
            tool_calls.append({"id": c["id"], "function": {"name": c["name"], "arguments": args}})
        return {"content": "".join(content), "tool_calls": tool_calls}


class Models:
    """The model mathbuddy talks to, on one of the configured servers.
    The app and the assistant hold this, so switching server happens in one place."""

    def __init__(self, servers: list):
        self.servers = servers
        self.server = servers[0]

    @property
    def model(self) -> str:
        return self.server.model

    @model.setter
    def model(self, name: str):
        self.server.model = name

    @property
    def base(self) -> str:
        return self.server.base

    @property
    def cpu(self) -> bool:
        return self.server.cpu

    @property
    def think(self) -> bool:
        return self.server.think

    @think.setter
    def think(self, on: bool):
        for s in self.servers:
            s.think = on

    def models(self) -> list[str]:
        return self.server.models()

    def use(self, server, name: str):
        server.model = name
        self.server = server

    def label(self) -> str:
        if self.cpu:
            return self.model + " (CPU)"
        return self.model + (" (llama.cpp)" if self.server.kind == "llama.cpp" else "")

    def catalog(self) -> list[tuple[object, list[tuple[str, bool]] | None, str]]:
        """Per server: (server, [(name, loaded)] or None, why it couldn't be listed)."""
        out = []
        for s in self.servers:
            try:
                out.append((s, s.catalog(), ""))
            except Exception as e:
                out.append((s, None, str(getattr(e, "reason", e))))
        return out

    def chat(self, *a, **kw) -> dict:
        return self.server.chat(*a, **kw)
