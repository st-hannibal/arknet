"""arknet Python SDK — OpenAI-compatible client for the arknet network.

Usage::

    from arknet import Client

    client = Client("http://127.0.0.1:3000")
    resp = client.chat_completion(
        model="meta-llama/Llama-3-8B",
        messages=[{"role": "user", "content": "Hello!"}],
    )
    print(resp["choices"][0]["message"]["content"])

Phase 4: published to PyPI as ``arknet``. Currently a thin HTTP wrapper;
PyO3 native bindings ship when performance-critical paths are identified.
"""

__version__ = "0.1.0"

from arknet.client import Client

__all__ = ["Client"]
