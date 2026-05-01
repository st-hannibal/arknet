"""Tests for the arknet Python SDK client."""

import json
import os
import threading
from http.server import HTTPServer, BaseHTTPRequestHandler
from unittest import TestCase

from arknet_sdk import Client


def _free_port():
    import socket
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class FakeHandler(BaseHTTPRequestHandler):
    """Minimal mock that records requests and returns canned responses."""

    requests = []

    def log_message(self, *_args):
        pass

    def do_GET(self):
        FakeHandler.requests.append(("GET", self.path, dict(self.headers)))
        if self.path == "/v1/models":
            self._json(200, {"data": [{"id": "test-model"}]})
        elif self.path == "/v1/gateways":
            self._json(200, {
                "gateways": [
                    {"url": f"http://127.0.0.1:{self.server.server_port}", "https": False},
                    {"url": "https://secure.example.com", "https": True},
                ]
            })
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length)) if length else {}
        FakeHandler.requests.append(("POST", self.path, dict(self.headers), body))
        if self.path == "/v1/chat/completions":
            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                chunk = {"choices": [{"delta": {"content": "hello"}, "index": 0, "finish_reason": None}]}
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                done_chunk = {"choices": [{"delta": {}, "index": 0, "finish_reason": "stop"}]}
                self.wfile.write(f"data: {json.dumps(done_chunk)}\n\n".encode())
                self.wfile.write(b"data: [DONE]\n\n")
            else:
                self._json(200, {
                    "id": "chatcmpl-test",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "world"},
                        "finish_reason": "stop",
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                })
        else:
            self._json(404, {"error": "not found"})

    def _json(self, status, obj):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class TestClient(TestCase):
    @classmethod
    def setUpClass(cls):
        cls.port = _free_port()
        cls.server = HTTPServer(("127.0.0.1", cls.port), FakeHandler)
        cls.thread = threading.Thread(target=cls.server.serve_forever)
        cls.thread.daemon = True
        cls.thread.start()
        cls.base = f"http://127.0.0.1:{cls.port}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        FakeHandler.requests.clear()

    def test_constructor_strips_trailing_slash(self):
        c = Client(f"{self.base}/", api_key="test")
        self.assertEqual(c.base_url, self.base)

    def test_api_key_from_param(self):
        c = Client(self.base, api_key="ark1abc")
        self.assertEqual(c.api_key, "ark1abc")

    def test_api_key_from_env(self):
        old = os.environ.get("ARKNET_WALLET")
        try:
            os.environ["ARKNET_WALLET"] = "ark1env"
            c = Client(self.base)
            self.assertEqual(c.api_key, "ark1env")
        finally:
            if old is None:
                os.environ.pop("ARKNET_WALLET", None)
            else:
                os.environ["ARKNET_WALLET"] = old

    def test_api_key_param_overrides_env(self):
        old = os.environ.get("ARKNET_WALLET")
        try:
            os.environ["ARKNET_WALLET"] = "ark1env"
            c = Client(self.base, api_key="ark1param")
            self.assertEqual(c.api_key, "ark1param")
        finally:
            if old is None:
                os.environ.pop("ARKNET_WALLET", None)
            else:
                os.environ["ARKNET_WALLET"] = old

    def test_auth_header_sent(self):
        c = Client(self.base, api_key="ark1secret")
        c.list_models()
        _, _, headers = FakeHandler.requests[-1]
        self.assertEqual(headers.get("Authorization"), "Bearer ark1secret")

    def test_no_auth_header_when_no_key(self):
        old = os.environ.pop("ARKNET_WALLET", None)
        try:
            c = Client(self.base)
            c.list_models()
            _, _, headers = FakeHandler.requests[-1]
            self.assertNotIn("Authorization", headers)
        finally:
            if old:
                os.environ["ARKNET_WALLET"] = old

    def test_chat_completion(self):
        c = Client(self.base, api_key="test")
        resp = c.chat_completion(
            model="test-model",
            messages=[{"role": "user", "content": "hi"}],
        )
        self.assertEqual(resp["choices"][0]["message"]["content"], "world")

    def test_chat_completion_sends_prefer_tee(self):
        c = Client(self.base, api_key="test")
        c.chat_completion(
            model="m",
            messages=[{"role": "user", "content": "x"}],
            prefer_tee=True,
        )
        _, _, _, body = FakeHandler.requests[-1]
        self.assertTrue(body.get("prefer_tee"))

    def test_chat_completion_sends_require_https(self):
        c = Client(self.base, api_key="test")
        c.chat_completion(
            model="m",
            messages=[{"role": "user", "content": "x"}],
            require_https=True,
        )
        _, _, _, body = FakeHandler.requests[-1]
        self.assertTrue(body.get("require_https"))

    def test_chat_completion_stream(self):
        c = Client(self.base, api_key="test")
        chunks = list(c.chat_completion_stream(
            model="m",
            messages=[{"role": "user", "content": "x"}],
        ))
        self.assertGreater(len(chunks), 0)
        self.assertIn("choices", chunks[0])

    def test_list_models(self):
        c = Client(self.base, api_key="test")
        resp = c.list_models()
        self.assertIn("data", resp)

    def test_connect_discovers_gateway(self):
        c = Client.connect(seed_urls=[self.base])
        self.assertIsNotNone(c.base_url)

    def test_connect_prefers_https(self):
        c = Client.connect(seed_urls=[self.base])
        self.assertIn("https://", c.base_url)

    def test_connect_require_https_filters(self):
        c = Client.connect(seed_urls=[self.base], require_https=True)
        self.assertTrue(c.base_url.startswith("https://"))

    def test_connect_no_gateways_raises(self):
        with self.assertRaises(RuntimeError):
            Client.connect(seed_urls=["http://127.0.0.1:1"])

    def test_connect_passes_api_key(self):
        c = Client.connect(seed_urls=[self.base], api_key="ark1test")
        self.assertEqual(c.api_key, "ark1test")
