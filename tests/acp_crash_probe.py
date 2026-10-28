#!/usr/bin/env python3
"""Probe ACP recovery with synthetic traffic and an isolated HOME. No real API calls."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import signal
import sqlite3
import subprocess
import threading
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BASELINE = "BASELINE_SYNTHETIC_COMPLETE"
ACTIVE = "ACTIVE_SYNTHETIC_USER"
STEP = "AUTHORITATIVE_SYNTHETIC_STEP"
TOOL = "SYNTHETIC_TOOL_RESULT_92814"
PARTIAL = "PARTIAL_SYNTHETIC_RESPONSE"
FINAL = "FINAL_SYNTHETIC_RESPONSE"


def binary_sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def durable_json(path, value):
    temporary = path.with_suffix(".tmp")
    with temporary.open("w") as handle:
        json.dump(value, handle, indent=2)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    temporary.replace(path)
    descriptor = os.open(path.parent, os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


class MockProvider:
    def __init__(self):
        self.tool_received = threading.Event()
        self.release = threading.Event()
        self.tool_marker_received = False
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                payload = json.dumps({"data": [{"id": "test/model", "context_length": 100000,
                    "supported_parameters": ["tools"], "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                messages = body["messages"]
                active = any(ACTIVE in json.dumps(message) for message in messages)
                user_index = max(i for i, message in enumerate(messages) if message["role"] == "user")
                tool_messages = [m for m in messages[user_index + 1:] if m["role"] == "tool"]
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True

                def chunk(delta, reason=None):
                    payload = {"choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
                        "usage": {"prompt_tokens": 80, "completion_tokens": 10}}
                    self.wfile.write(("data: " + json.dumps(payload) + "\n\n").encode())
                    self.wfile.flush()

                try:
                    if not active:
                        chunk({"role": "assistant", "content": BASELINE}, "stop")
                    elif not tool_messages:
                        chunk({"role": "assistant", "content": STEP, "tool_calls": [{"index": 0,
                            "id": "synthetic-call-1", "type": "function", "function": {
                            "name": "read_file", "arguments": json.dumps({"path": "probe.txt"})}}]}, "tool_calls")
                    else:
                        owner.tool_marker_received = TOOL in json.dumps(tool_messages)
                        chunk({"role": "assistant", "content": PARTIAL})
                        owner.tool_received.set()
                        if not owner.release.wait(45):
                            return
                        chunk({"content": FINAL}, "stop")
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)


class ACP:
    def __init__(self, binary, case_dir, provider, suffix):
        self.notifications = []
        self.responses = queue.Queue()
        self.pending = {}
        self.number = 0
        workspace = case_dir / "workspace"
        home = case_dir / "home"
        workspace.mkdir(exist_ok=True)
        home.mkdir(exist_ok=True)
        (workspace / "probe.txt").write_text(TOOL + "\n")
        config = case_dir / "config.toml"
        config.write_text("[analytics]\nenabled=false\n[acp.mcp]\nenabled=false\n")
        agent_env = case_dir / "agent.env"
        agent_env.write_text("\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter", "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.server.server_port}",
            "DAIMONOS_AGENT_API_KEY=synthetic-not-a-real-key", "DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "DAIMONOS_AGENT_COMPACTION=off", "DAIMONOS_AGENT_ALLOWED_COMMANDS=read_file", ""]))
        self.stderr = (case_dir / f"stderr-{suffix}.log").open("w")
        environment = {k: v for k, v in os.environ.items() if not k.startswith(("DAIMONOS_", "XDG_"))}
        environment.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"))
        self.process = subprocess.Popen([str(binary), "--workspace", str(workspace), "--config", str(config),
            "acp", "--agent-env", str(agent_env)], cwd=workspace, env=environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, text=True, bufsize=1)
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()
        self.request("initialize", {"protocolVersion": 1, "clientCapabilities": {},
            "clientInfo": {"name": "synthetic-crash-probe", "version": "1"}})

    def read(self):
        for line in self.process.stdout:
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "id" in message and "method" not in message:
                self.responses.put(message)
            elif "id" in message:
                self.send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "probe does not support client requests"}})
            else:
                self.notifications.append(message)
        self.responses.put(None)

    def send(self, value):
        self.process.stdin.write(json.dumps(value) + "\n")
        self.process.stdin.flush()

    def begin(self, method, params):
        self.number += 1
        self.send({"jsonrpc": "2.0", "id": self.number, "method": method, "params": params})
        return self.number

    def wait(self, number, timeout=30):
        deadline = time.monotonic() + timeout
        while number not in self.pending:
            message = self.responses.get(timeout=max(0.01, deadline - time.monotonic()))
            if message is None:
                raise RuntimeError(f"ACP exited early: {self.process.poll()}")
            self.pending[message["id"]] = message
        message = self.pending.pop(number)
        if "error" in message:
            raise RuntimeError(f"ACP RPC error: {message['error']}")
        return message["result"]

    def request(self, method, params):
        return self.wait(self.begin(method, params))

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.thread.join(timeout=2)
        self.stderr.close()


def markers(text):
    return {"baseline": BASELINE in text, "active_user": ACTIVE in text,
        "assistant_step": STEP in text, "tool_result": TOOL in text,
        "partial": PARTIAL in text, "final": FINAL in text}


def storage(case_dir, session_id):
    directory = case_dir / "home" / ".daimonos" / "acp-sessions"
    database = directory / "sessions.sqlite3"
    if database.exists():
        with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
            row = connection.execute("SELECT payload FROM sessions WHERE session_id=?", (session_id,)).fetchone()
            integrity = connection.execute("PRAGMA quick_check").fetchone()[0]
        payload = json.loads(row[0]) if row else {}
        return {"format": "sqlite", "integrity": integrity,
            "message_count": len(payload.get("messages", [])), **markers(json.dumps(payload))}
    path = directory / (session_id + ".json")
    if path.exists():
        payload = json.loads(path.read_text())
        return {"format": "json", "message_count": len(payload.get("messages", [])), **markers(json.dumps(payload))}
    return {"format": "absent", **markers("")}


def experiment(binary, root, case):
    directory = root / case
    directory.mkdir()
    provider = MockProvider()
    client = None
    restored = None
    result = {"case": case}
    try:
        client = ACP(binary, directory, provider, "original")
        workspace = str(directory / "workspace")
        session_id = client.request("session/new", {"cwd": workspace, "mcpServers": []})["sessionId"]
        params = {"sessionId": session_id, "prompt": [{"type": "text", "text": "baseline synthetic request"}]}
        client.request("session/prompt", params)
        result["baseline_store"] = storage(directory, session_id)
        if not result["baseline_store"]["baseline"]:
            raise RuntimeError("completed baseline was not present in storage")
        pending = None
        if case != "completed_kill":
            params["prompt"] = [{"type": "text", "text": ACTIVE}]
            pending = client.begin("session/prompt", params)
            if not provider.tool_received.wait(30):
                raise RuntimeError("provider did not receive the authoritative tool result")
            result["provider_received_tool_result"] = provider.tool_marker_received
            if not provider.tool_marker_received:
                raise RuntimeError("provider request did not contain the synthetic tool result")
            result["before_shutdown_store"] = storage(directory, session_id)
        if case in {"completed_kill", "inflight_kill"}:
            client.process.kill()
            client.process.wait(timeout=5)
        elif case == "inflight_term":
            client.process.send_signal(signal.SIGTERM)
            client.process.wait(timeout=12)
        elif case == "inflight_eof":
            client.process.stdin.close()
            client.process.wait(timeout=12)
        elif case == "inflight_cancel":
            client.send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}})
            result["cancel_response"] = client.wait(pending)
        elif case == "finished":
            provider.release.set()
            result["completion_response"] = client.wait(pending)
        result["original_exit_before_cleanup"] = client.process.poll()
        client.close()
        result["after_shutdown_store"] = storage(directory, session_id)
        provider.release.set()
        restored = ACP(binary, directory, provider, "restored")
        restored.request("session/load", {"sessionId": session_id, "cwd": workspace, "mcpServers": []})
        # The load response follows the queued replay notifications on stdout.
        result["replayed"] = markers(json.dumps(restored.notifications))
        result["session_id"] = session_id
    except Exception as error:
        result["error"] = f"{type(error).__name__}: {error}"
    finally:
        provider.release.set()
        if restored:
            restored.close()
        if client:
            client.close()
        provider.close()
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--cases", default="completed_kill,inflight_kill,inflight_term,inflight_eof,inflight_cancel,finished")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = args.output_root.resolve()
    root.mkdir(parents=True, exist_ok=False)
    report = {"started_utc": datetime.now(timezone.utc).isoformat(), "binary": str(binary),
        "binary_sha256": binary_sha256(binary), "cases": []}
    durable_json(root / "results.json", report)
    for case in args.cases.split(","):
        if case not in {"completed_kill", "inflight_kill", "inflight_term", "inflight_eof", "inflight_cancel", "finished"}:
            parser.error("unknown case")
        result = experiment(binary, root, case)
        report["cases"].append(result)
        durable_json(root / "results.json", report)
        print(json.dumps(result), flush=True)
    return int(any("error" in result for result in report["cases"]))


if __name__ == "__main__":
    raise SystemExit(main())
