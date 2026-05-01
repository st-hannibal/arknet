import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

// Import the compiled SDK.
import { ArknetClient, Wallet } from "../dist/index.js";

// ─── Mock server ──────────────────────────────────────────────────

const requests = [];

function createMockServer() {
  return http.createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      const parsed = body ? JSON.parse(body) : null;
      requests.push({
        method: req.method,
        path: req.url,
        headers: req.headers,
        body: parsed,
      });

      if (req.url === "/v1/models" && req.method === "GET") {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ data: [{ id: "test-model", owned_by: "test" }] }));
      } else if (req.url === "/v1/gateways" && req.method === "GET") {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({
          gateways: [
            { url: `http://127.0.0.1:${server.address().port}`, https: false },
            { url: "https://secure.example.com", https: true },
          ],
        }));
      } else if (req.url === "/v1/chat/completions" && req.method === "POST") {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({
          id: "chatcmpl-test",
          object: "chat.completion",
          choices: [{
            index: 0,
            message: { role: "assistant", content: "hello" },
            finish_reason: "stop",
          }],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        }));
      } else {
        res.writeHead(404);
        res.end("not found");
      }
    });
  });
}

let server;
let base;

before(async () => {
  server = createMockServer();
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  base = `http://127.0.0.1:${server.address().port}`;
});

after(() => {
  server.close();
});

// ─── Tests ────────────────────────────────────────────────────────

describe("ArknetClient", () => {
  it("strips trailing slash from base URL", () => {
    const c = new ArknetClient(`${base}/`);
    assert.ok(!c["baseUrl"].endsWith("/"));
  });

  it("reads ARKNET_WALLET from env", () => {
    process.env.ARKNET_WALLET = "ark1env";
    const c = new ArknetClient(base);
    assert.equal(c["apiKey"], "ark1env");
    delete process.env.ARKNET_WALLET;
  });

  it("explicit apiKey overrides env", () => {
    process.env.ARKNET_WALLET = "ark1env";
    const c = new ArknetClient(base, "ark1param");
    assert.equal(c["apiKey"], "ark1param");
    delete process.env.ARKNET_WALLET;
  });

  it("sends Authorization header when api_key set", async () => {
    requests.length = 0;
    const c = new ArknetClient(base, "ark1secret");
    await c.listModels();
    assert.equal(requests.at(-1).headers.authorization, "Bearer ark1secret");
  });

  it("does not send Authorization when no key", async () => {
    delete process.env.ARKNET_WALLET;
    requests.length = 0;
    const c = new ArknetClient(base);
    await c.listModels();
    assert.equal(requests.at(-1).headers.authorization, undefined);
  });

  it("chatCompletion returns response", async () => {
    const c = new ArknetClient(base, "test");
    const resp = await c.chatCompletion({
      model: "test-model",
      messages: [{ role: "user", content: "hi" }],
    });
    assert.equal(resp.choices[0].message.content, "hello");
  });

  it("chatCompletion sends prefer_tee", async () => {
    requests.length = 0;
    const c = new ArknetClient(base, "test");
    await c.chatCompletion({
      model: "m",
      messages: [{ role: "user", content: "x" }],
      prefer_tee: true,
    });
    assert.equal(requests.at(-1).body.prefer_tee, true);
  });

  it("chatCompletion sends require_https", async () => {
    requests.length = 0;
    const c = new ArknetClient(base, "test");
    await c.chatCompletion({
      model: "m",
      messages: [{ role: "user", content: "x" }],
      require_https: true,
    });
    assert.equal(requests.at(-1).body.require_https, true);
  });

  it("listModels returns data", async () => {
    const c = new ArknetClient(base, "test");
    const resp = await c.listModels();
    assert.ok(resp.data.length > 0);
    assert.equal(resp.data[0].id, "test-model");
  });
});

describe("ArknetClient.connect", () => {
  it("discovers gateway from seed", async () => {
    const c = await ArknetClient.connect({ seeds: [base] });
    assert.ok(c instanceof ArknetClient);
  });

  it("prefers HTTPS gateway", async () => {
    const c = await ArknetClient.connect({ seeds: [base] });
    assert.ok(c["baseUrl"].startsWith("https://"));
  });

  it("requireHttps filters HTTP gateways", async () => {
    const c = await ArknetClient.connect({ seeds: [base], requireHttps: true });
    assert.ok(c["baseUrl"].startsWith("https://"));
  });

  it("throws when no gateways reachable", async () => {
    await assert.rejects(
      () => ArknetClient.connect({ seeds: ["http://127.0.0.1:1"] }),
      /no reachable gateway/
    );
  });

  it("passes apiKey to discovered client", async () => {
    const c = await ArknetClient.connect({ seeds: [base], apiKey: "ark1test" });
    assert.equal(c["apiKey"], "ark1test");
  });

  it("passes wallet to discovered client", async () => {
    const wallet = Wallet.create();
    const c = await ArknetClient.connect({ seeds: [base], wallet });
    assert.equal(c.getWallet(), wallet);
  });
});

// ─── Wallet tests ────────────────────────────────────────────────

describe("Wallet", () => {
  it("create produces valid wallet", () => {
    const w = Wallet.create();
    assert.equal(w.publicKey.length, 32);
    assert.ok(w.address.startsWith("0x"));
    // "0x" + 40 hex chars = 42 chars total (20 bytes)
    assert.equal(w.address.length, 42);
  });

  it("fromSeed is deterministic", () => {
    const seed = new Uint8Array(32).fill(0x42);
    const w1 = Wallet.fromSeed(seed);
    const w2 = Wallet.fromSeed(seed);
    assert.deepEqual(w1.publicKey, w2.publicKey);
    assert.equal(w1.address, w2.address);
  });

  it("different seeds produce different addresses", () => {
    const w1 = Wallet.fromSeed(new Uint8Array(32).fill(0x01));
    const w2 = Wallet.fromSeed(new Uint8Array(32).fill(0x02));
    assert.notEqual(w1.address, w2.address);
  });

  it("fromSeed rejects wrong seed length", () => {
    assert.throws(
      () => Wallet.fromSeed(new Uint8Array(16)),
      /seed must be 32 bytes/
    );
  });

  it("sign and verify roundtrip", () => {
    const w = Wallet.create();
    const msg = new TextEncoder().encode("arknet test message");
    const sig = w.sign(msg);
    assert.equal(sig.length, 64); // Ed25519 signature is 64 bytes
    assert.ok(w.verify(msg, sig));
  });

  it("verify rejects tampered message", () => {
    const w = Wallet.create();
    const msg = new TextEncoder().encode("original");
    const sig = w.sign(msg);
    const tampered = new TextEncoder().encode("tampered");
    assert.ok(!w.verify(tampered, sig));
  });

  it("save and load roundtrip", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "arknet-wallet-test-"));
    const filePath = path.join(tmpDir, "test.key");
    try {
      const w1 = Wallet.create();
      w1.save(filePath);

      // Verify file is 64 bytes.
      const stat = fs.statSync(filePath);
      assert.equal(stat.size, 64);

      const w2 = Wallet.load(filePath);
      assert.deepEqual(w2.publicKey, w1.publicKey);
      assert.equal(w2.address, w1.address);

      // Sign with loaded wallet, verify with original.
      const msg = new TextEncoder().encode("cross-check");
      const sig = w2.sign(msg);
      assert.ok(w1.verify(msg, sig));
    } finally {
      fs.rmSync(tmpDir, { recursive: true });
    }
  });

  it("save creates parent directories", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "arknet-wallet-test-"));
    const filePath = path.join(tmpDir, "deep", "nested", "wallet.key");
    try {
      const w = Wallet.create();
      w.save(filePath);
      assert.ok(fs.existsSync(filePath));
    } finally {
      fs.rmSync(tmpDir, { recursive: true });
    }
  });

  it("load rejects wrong file size", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "arknet-wallet-test-"));
    const filePath = path.join(tmpDir, "bad.key");
    try {
      fs.writeFileSync(filePath, Buffer.alloc(32));
      assert.throws(
        () => Wallet.load(filePath),
        /wallet file must be 64 bytes/
      );
    } finally {
      fs.rmSync(tmpDir, { recursive: true });
    }
  });

  it("load rejects mismatched public key", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "arknet-wallet-test-"));
    const filePath = path.join(tmpDir, "mismatch.key");
    try {
      const w = Wallet.create();
      w.save(filePath);
      // Corrupt the public key half (bytes 32-63).
      const buf = fs.readFileSync(filePath);
      buf.fill(0xff, 32, 64);
      fs.writeFileSync(filePath, buf);
      assert.throws(
        () => Wallet.load(filePath),
        /does not match/
      );
    } finally {
      fs.rmSync(tmpDir, { recursive: true });
    }
  });

  it("defaultPath uses ARKNET_WALLET_PATH env var", () => {
    const custom = "/tmp/custom_wallet.key";
    process.env.ARKNET_WALLET_PATH = custom;
    assert.equal(Wallet.defaultPath(), custom);
    delete process.env.ARKNET_WALLET_PATH;
  });

  it("defaultPath falls back to ~/.arknet/wallet.key", () => {
    delete process.env.ARKNET_WALLET_PATH;
    const expected = path.join(os.homedir(), ".arknet", "wallet.key");
    assert.equal(Wallet.defaultPath(), expected);
  });
});

describe("ArknetClient with wallet", () => {
  it("accepts wallet in constructor options", () => {
    const wallet = Wallet.create();
    const c = new ArknetClient(base, { wallet });
    assert.equal(c.getWallet(), wallet);
  });

  it("accepts legacy string apiKey constructor", () => {
    const c = new ArknetClient(base, "ark1legacy");
    assert.equal(c["apiKey"], "ark1legacy");
  });

  it("accepts apiKey and wallet together", () => {
    const wallet = Wallet.create();
    const c = new ArknetClient(base, { apiKey: "ark1both", wallet });
    assert.equal(c["apiKey"], "ark1both");
    assert.equal(c.getWallet(), wallet);
  });
});
