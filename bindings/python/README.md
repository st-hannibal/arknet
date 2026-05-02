# arknet-sdk

Python SDK for the arknet decentralized AI inference network.

```python
from arknet_sdk import Wallet, Client

wallet = Wallet.create()
wallet.save()

client = Client("http://gateway:3000", wallet=wallet)
response = client.chat_completion(
    model="Qwen/Qwen3-0.6B-Q4_K_M",
    messages=[{"role": "user", "content": "Hello!"}],
)
print(response["choices"][0]["message"]["content"])
```
