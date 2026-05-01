"""arknet Python SDK — OpenAI-compatible client for the arknet network.

Usage::

    from arknet_sdk import Client

    client = Client.connect()
    resp = client.chat_completion(
        model="meta-llama/Llama-3.1-8B-Instruct",
        messages=[{"role": "user", "content": "Hello!"}],
    )
    print(resp["choices"][0]["message"]["content"])
"""

__version__ = "0.1.0"

from arknet_sdk.client import Client

__all__ = ["Client"]
