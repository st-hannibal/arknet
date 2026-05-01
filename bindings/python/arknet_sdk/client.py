"""Thin HTTP client for the arknet OpenAI-compatible API."""

from __future__ import annotations

import json
import os
from typing import Any, Dict, Iterator, List, Optional
from urllib.request import Request, urlopen
from urllib.error import HTTPError


_DEFAULT_SEEDS = [
    "https://api.arknet.arkengel.com",
]


class Client:
    """arknet API client.

    Parameters
    ----------
    base_url:
        Node HTTP root, e.g. ``"http://127.0.0.1:3000"``.
    api_key:
        Optional API key (unused in Phase 1; placeholder for Phase 4
        wallet-session tokens).
    """

    def __init__(self, base_url: str, api_key: Optional[str] = None) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key or os.environ.get("ARKNET_WALLET")

    @classmethod
    def connect(
        cls,
        seed_urls: Optional[List[str]] = None,
        require_https: bool = False,
        api_key: Optional[str] = None,
    ) -> "Client":
        """Auto-discover a gateway from the on-chain registry.

        Contacts each seed URL's ``/v1/gateways`` endpoint, picks the
        first reachable gateway (preferring HTTPS), and returns a
        connected Client.

        Parameters
        ----------
        seed_urls:
            Bootstrap URLs to discover gateways. Defaults to the arknet
            seed list.
        require_https:
            Only connect to HTTPS gateways. Raises if none available.
        """
        seeds = seed_urls or _DEFAULT_SEEDS
        for seed in seeds:
            try:
                url = f"{seed.rstrip('/')}/v1/gateways"
                req = Request(url, method="GET")
                with urlopen(req, timeout=10) as resp:
                    data = json.loads(resp.read())
                gateways = data.get("gateways", [])
                # Sort HTTPS first.
                gateways.sort(key=lambda g: (not g.get("https", False)))
                for gw in gateways:
                    if require_https and not gw.get("https", False):
                        continue
                    return cls(gw["url"], api_key=api_key)
            except Exception:
                continue
        if require_https:
            raise RuntimeError("no HTTPS gateway found in any seed")
        raise RuntimeError("no reachable gateway found")

    def _headers(self) -> Dict[str, str]:
        h: Dict[str, str] = {"Content-Type": "application/json"}
        if self.api_key:
            h["Authorization"] = f"Bearer {self.api_key}"
        return h

    def chat_completion(
        self,
        model: str,
        messages: List[Dict[str, str]],
        max_tokens: int = 256,
        temperature: float = 1.0,
        stream: bool = False,
        stop: Optional[List[str]] = None,
        prefer_tee: bool = False,
        require_https: bool = False,
    ) -> Dict[str, Any]:
        """Non-streaming chat completion.

        Parameters
        ----------
        prefer_tee:
            Route only to TEE-capable nodes for confidential inference.
            Prompts are encrypted to the enclave's public key — the host
            OS never sees plaintext. Rejected if no TEE node is available
            (no silent downgrade).
        require_https:
            Route only through HTTPS gateways. Protects the last mile
            (user to gateway) with TLS. Rejected if no HTTPS gateway
            is available (no silent downgrade to HTTP).

        Returns the full OpenAI-shaped response dict.
        """
        body = {
            "model": model,
            "messages": messages,
            "max_tokens": max_tokens,
            "temperature": temperature,
            "stream": stream,
        }
        if stop:
            body["stop"] = stop
        if prefer_tee:
            body["prefer_tee"] = True
        if require_https:
            body["require_https"] = True
        return self._post("/v1/chat/completions", body)

    def chat_completion_stream(
        self,
        model: str,
        messages: List[Dict[str, str]],
        max_tokens: int = 256,
        temperature: float = 1.0,
        stop: Optional[List[str]] = None,
        prefer_tee: bool = False,
        require_https: bool = False,
    ) -> Iterator[Dict[str, Any]]:
        """Streaming chat completion — yields SSE chunks as dicts.

        Parameters
        ----------
        prefer_tee:
            Route only to TEE-capable nodes. See :meth:`chat_completion`.
        require_https:
            Route only through HTTPS gateways. See :meth:`chat_completion`.
        """
        body = {
            "model": model,
            "messages": messages,
            "max_tokens": max_tokens,
            "temperature": temperature,
            "stream": True,
        }
        if stop:
            body["stop"] = stop
        if prefer_tee:
            body["prefer_tee"] = True
        if require_https:
            body["require_https"] = True
        return self._post_stream("/v1/chat/completions", body)

    def list_models(self) -> Dict[str, Any]:
        """List registered models."""
        return self._get("/v1/models")

    # ── Internal ─────────────────────────────────────────────────────

    def _get(self, path: str) -> Dict[str, Any]:
        url = f"{self.base_url}{path}"
        req = Request(url, headers=self._headers(), method="GET")
        try:
            with urlopen(req, timeout=30) as resp:
                return json.loads(resp.read())
        except HTTPError as e:
            raise RuntimeError(
                f"arknet API error ({e.code}): {e.read().decode()}"
            ) from e

    def _post(self, path: str, body: Dict[str, Any]) -> Dict[str, Any]:
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode()
        req = Request(url, data=data, headers=self._headers(), method="POST")
        try:
            with urlopen(req, timeout=120) as resp:
                return json.loads(resp.read())
        except HTTPError as e:
            raise RuntimeError(
                f"arknet API error ({e.code}): {e.read().decode()}"
            ) from e

    def _post_stream(
        self, path: str, body: Dict[str, Any]
    ) -> Iterator[Dict[str, Any]]:
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode()
        req = Request(url, data=data, headers=self._headers(), method="POST")
        try:
            resp = urlopen(req, timeout=120)
        except HTTPError as e:
            raise RuntimeError(
                f"arknet API error ({e.code}): {e.read().decode()}"
            ) from e

        for line in resp:
            text = line.decode().strip()
            if not text or not text.startswith("data:"):
                continue
            payload = text[len("data:"):].strip()
            if payload == "[DONE]":
                break
            yield json.loads(payload)
