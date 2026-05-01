import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";

// Import the compiled SDK.
import { ArknetClient } from "../dist/index.js";

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
});
