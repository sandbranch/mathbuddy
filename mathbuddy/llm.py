"""Minimal Ollama /api/chat client: streaming, tool calls, CPU-only switch.

Plain urllib on purpose, so mathbuddy needs nothing beyond what a Jupyter
install already has.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request


def ollama_base(host: str | None = None) -> str:
    host = host or os.environ.get("OLLAMA_HOST") or "127.0.0.1:11434"
    if "://" not in host:
        host = "http://" + host
    if host.startswith("http://0.0.0.0"):
        host = host.replace("0.0.0.0", "127.0.0.1", 1)
    return host.rstrip("/")


class Ollama:
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
        with urllib.request.urlopen(self.base + "/api/tags", timeout=5) as r:
            return [m["name"] for m in json.load(r).get("models", [])]

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
        req = urllib.request.Request(
            self.base + "/api/chat",
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
        )
        content, calls = [], []
        try:
            resp = urllib.request.urlopen(req, timeout=600)
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"ollama: HTTP {e.code}: {e.read().decode(errors='replace')[:300]}") from None
        except urllib.error.URLError as e:
            raise RuntimeError(f"ollama: cannot reach {self.base} ({e.reason})") from None
        with resp:
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
