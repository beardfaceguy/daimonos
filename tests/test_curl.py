"""MCP-level tests for the curl tool plugin (#37)."""

import http.server
import json
import threading


def _spawn_server(handler_class, requests=1):
    """Start a minimal HTTP server on a random port. Returns (server, port)."""
    server = http.server.HTTPServer(("127.0.0.1", 0), handler_class)
    port = server.server_address[1]

    def serve():
        for _ in range(requests):
            server.handle_request()

    t = threading.Thread(target=serve, daemon=True)
    t.start()
    return server, port


def _payload(result):
    return json.loads(result["content"][0]["text"])


# --- Handler classes ---

class _OkHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"hello from test"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Test", "pytest")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class _PostEchoHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        self.send_response(201)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class _HeaderCapture(http.server.BaseHTTPRequestHandler):
    received_headers = {}

    def do_GET(self):
        _HeaderCapture.received_headers = dict(self.headers)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


class _NotFoundHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"not found"
        self.send_response(404)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


# --- Tests ---

def test_curl_in_tool_list(daimonos):
    tools = daimonos.list_tools()
    names = [t["name"] for t in tools]
    assert "curl" in names, f"curl tool not in list; got: {names}"


def test_curl_params_accepted_without_error(daimonos):
    # Verifies the tool accepts all documented params (schema is terse in list_tools)
    # Use an unreachable host; we just check that params are forwarded without an RPC error
    result = daimonos.call_tool("curl", {
        "url": "http://127.0.0.1:1",
        "method": "GET",
        "headers": {"X-Test": "value"},
        "timeout": 1,
    })
    payload = json.loads(result["content"][0]["text"])
    # Either an error (connection refused) or a status — no RPC-level crash
    assert "error" in payload or "status" in payload, f"unexpected payload: {payload}"


def test_curl_get_returns_status_and_body(daimonos):
    _, port = _spawn_server(_OkHandler)

    result = daimonos.call_tool("curl", {"url": f"http://127.0.0.1:{port}"})
    payload = _payload(result)

    assert payload["status"] == 200, f"expected 200, got {payload}"
    assert payload["body"] == "hello from test"
    assert payload["method"] == "GET"
    assert "timing_ms" in payload
    assert payload["timing_ms"] >= 0


def test_curl_response_headers_present(daimonos):
    _, port = _spawn_server(_OkHandler)

    result = daimonos.call_tool("curl", {"url": f"http://127.0.0.1:{port}"})
    payload = _payload(result)

    headers = payload.get("headers", {})
    assert "content-type" in headers, f"expected content-type header; got {headers}"
    assert "x-test" in headers
    assert headers["x-test"] == "pytest"


def test_curl_post_with_body(daimonos):
    _, port = _spawn_server(_PostEchoHandler)

    result = daimonos.call_tool("curl", {
        "url": f"http://127.0.0.1:{port}",
        "method": "POST",
        "body": "payload=hello",
    })
    payload = _payload(result)

    assert payload["status"] == 201
    assert payload["method"] == "POST"
    assert "payload=hello" in payload["body"]


def test_curl_custom_headers_forwarded(daimonos):
    _HeaderCapture.received_headers = {}
    _, port = _spawn_server(_HeaderCapture)

    daimonos.call_tool("curl", {
        "url": f"http://127.0.0.1:{port}",
        "headers": {"X-Custom-Header": "test-value-42"},
    })

    assert "X-Custom-Header" in _HeaderCapture.received_headers, (
        f"custom header not forwarded; got: {_HeaderCapture.received_headers}"
    )
    assert _HeaderCapture.received_headers["X-Custom-Header"] == "test-value-42"


def test_curl_404_returns_status(daimonos):
    _, port = _spawn_server(_NotFoundHandler)

    result = daimonos.call_tool("curl", {"url": f"http://127.0.0.1:{port}"})
    payload = _payload(result)

    assert payload["status"] == 404
    assert "not found" in payload["body"]


def test_curl_unreachable_returns_error(daimonos):
    # Port 1 is reserved and connection will be refused
    result = daimonos.call_tool("curl", {
        "url": "http://127.0.0.1:1",
        "timeout": 2,
    })
    payload = _payload(result)

    assert "error" in payload, f"expected error field for unreachable host; got {payload}"


def test_curl_url_echoed_in_response(daimonos):
    _, port = _spawn_server(_OkHandler)
    url = f"http://127.0.0.1:{port}"

    result = daimonos.call_tool("curl", {"url": url})
    payload = _payload(result)

    assert payload["url"] == url
