/**
 * @arknet/sdk — TypeScript SDK for the arknet decentralized AI inference network.
 *
 * OpenAI-compatible client: point `baseUrl` at an arknet node and use
 * the same API shape as the OpenAI TypeScript SDK.
 *
 * @example
 * ```ts
 * import { ArknetClient, Wallet } from "@arknet/sdk";
 *
 * const wallet = Wallet.create();
 * wallet.save();
 *
 * const client = new ArknetClient("http://127.0.0.1:3000", { wallet });
 * const resp = await client.chatCompletion({
 *   model: "meta-llama/Llama-3-8B",
 *   messages: [{ role: "user", content: "Hello!" }],
 * });
 * console.log(resp.choices[0].message.content);
 * ```
 *
 * Phase 4: published to npm as `@arknet/sdk`. Currently a thin HTTP
 * wrapper; WASM bindings ship when client-side verification is needed.
 */

import nacl from "tweetnacl";
import { blake3 } from "@noble/hashes/blake3";
import * as fs from "fs";
import * as path from "path";
import * as os from "os";

// ─── Wallet ─────────────────────────────────────────────────────────

/**
 * Ed25519 wallet for signing arknet inference requests.
 *
 * The wallet holds an Ed25519 keypair (via tweetnacl) and derives the
 * on-chain address using `blake3(pubkey_bytes)[0..20]`, matching the
 * Rust SDK and CLI exactly.
 *
 * # File format
 *
 * The on-disk format is 64 bytes: `32 secret_seed || 32 public_key`.
 * This is identical to the Rust SDK (`~/.arknet/wallet.key`) so
 * wallets are cross-compatible between Rust CLI and TypeScript SDK.
 *
 * # Default path
 *
 * `~/.arknet/wallet.key`, overridable via `ARKNET_WALLET_PATH`.
 */
export class Wallet {
  private secretKey: Uint8Array; // 64 bytes (nacl format: seed || public)
  /** The 32-byte Ed25519 public key. */
  readonly publicKey: Uint8Array; // 32 bytes
  /** The 20-byte on-chain address as a "0x"-prefixed hex string. */
  readonly address: string; // "0x..." 40 hex chars

  private constructor(keypair: { publicKey: Uint8Array; secretKey: Uint8Array }) {
    this.secretKey = keypair.secretKey;
    this.publicKey = keypair.publicKey;
    // address = blake3(publicKey)[0..20] as hex with 0x prefix
    const hash = blake3(keypair.publicKey);
    this.address = "0x" + Buffer.from(hash.slice(0, 20)).toString("hex");
  }

  /** Generate a new wallet with a random Ed25519 keypair. */
  static create(): Wallet {
    return new Wallet(nacl.sign.keyPair());
  }

  /**
   * Create a wallet deterministically from a 32-byte seed.
   *
   * Useful for test fixtures and HKDF-derived wallets.
   */
  static fromSeed(seed: Uint8Array): Wallet {
    if (seed.length !== 32) {
      throw new Error(`seed must be 32 bytes, got ${seed.length}`);
    }
    return new Wallet(nacl.sign.keyPair.fromSeed(seed));
  }

  /**
   * Load a wallet from a 64-byte key file (32 secret_seed || 32 public_key).
   *
   * Returns an error if the file doesn't exist, has the wrong size,
   * or the public key doesn't match the secret seed.
   */
  static load(filePath?: string): Wallet {
    const p = filePath ?? Wallet.defaultPath();
    const bytes = fs.readFileSync(p);
    if (bytes.length !== 64) {
      throw new Error(`wallet file must be 64 bytes, got ${bytes.length}`);
    }
    // Format: 32 secret seed || 32 public key
    const seed = bytes.slice(0, 32);
    const wallet = Wallet.fromSeed(seed);

    // Verify the stored public key matches the derived one.
    const storedPub = bytes.slice(32, 64);
    for (let i = 0; i < 32; i++) {
      if (storedPub[i] !== wallet.publicKey[i]) {
        throw new Error("stored public key does not match secret key");
      }
    }

    return wallet;
  }

  /**
   * Save the wallet to a 64-byte key file (32 seed || 32 public).
   *
   * Creates parent directories if they don't exist. Sets file
   * permissions to owner-only (0o600) on Unix.
   */
  save(filePath?: string): void {
    const p = filePath ?? Wallet.defaultPath();
    fs.mkdirSync(path.dirname(p), { recursive: true });
    // Save as 64 bytes: 32 seed || 32 public
    // nacl secretKey is 64 bytes (seed || public), first 32 are the seed
    const buf = Buffer.alloc(64);
    buf.set(this.secretKey.slice(0, 32), 0);
    buf.set(this.publicKey, 32);
    fs.writeFileSync(p, buf, { mode: 0o600 });
  }

  /**
   * Sign an arbitrary message with the wallet's Ed25519 key.
   *
   * Returns a 64-byte detached Ed25519 signature.
   */
  sign(message: Uint8Array): Uint8Array {
    return nacl.sign.detached(message, this.secretKey);
  }

  /**
   * Verify a detached Ed25519 signature against this wallet's public key.
   */
  verify(message: Uint8Array, signature: Uint8Array): boolean {
    return nacl.sign.detached.verify(message, signature, this.publicKey);
  }

  /**
   * Resolve the default wallet path.
   *
   * Checks `ARKNET_WALLET_PATH` env var first, then falls back to
   * `~/.arknet/wallet.key`.
   */
  static defaultPath(): string {
    return (
      process.env.ARKNET_WALLET_PATH ??
      path.join(os.homedir(), ".arknet", "wallet.key")
    );
  }
}

// ─── Types ──────────────────────────────────────────────────────────

export interface ChatMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

export interface ChatCompletionRequest {
  model: string;
  messages: ChatMessage[];
  max_tokens?: number;
  temperature?: number;
  stream?: boolean;
  stop?: string | string[];
  /**
   * arknet extension: route only to TEE-capable nodes for confidential
   * inference. Prompts are encrypted to the enclave's public key — the
   * host OS never sees plaintext. Rejected if no TEE node is available
   * (no silent downgrade).
   */
  prefer_tee?: boolean;
  /**
   * arknet extension: route only through HTTPS gateways. Protects the
   * last mile (user to gateway) with TLS. Rejected if no HTTPS gateway
   * is available (no silent downgrade to HTTP).
   */
  require_https?: boolean;
}

export interface ChatCompletionResponse {
  id: string;
  object: string;
  created: number;
  model: string;
  choices: Array<{
    index: number;
    message: ChatMessage;
    finish_reason: string | null;
  }>;
  usage: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  };
}

export interface ChatCompletionChunk {
  id: string;
  object: string;
  created: number;
  model: string;
  choices: Array<{
    index: number;
    delta: { role?: string; content?: string };
    finish_reason: string | null;
  }>;
}

export interface ModelEntry {
  id: string;
  object: string;
  created: number;
  owned_by: string;
}

export interface ModelsResponse {
  object: string;
  data: ModelEntry[];
}

// ─── Client ─────────────────────────────────────────────────────────

const SEEDS_JSON_URL = "https://arknet.arkengel.com/seeds.json";
const FALLBACK_SEEDS = ["https://api.arknet.arkengel.com"];

async function fetchSeeds(): Promise<string[]> {
  try {
    const resp = await fetch(SEEDS_JSON_URL);
    if (!resp.ok) return FALLBACK_SEEDS;
    const data = (await resp.json()) as { seeds?: Array<{ url?: string }> };
    const urls = (data.seeds ?? []).map((s) => s.url).filter(Boolean) as string[];
    return urls.length > 0 ? urls : FALLBACK_SEEDS;
  } catch {
    return FALLBACK_SEEDS;
  }
}

export class ArknetClient {
  private baseUrl: string;
  private apiKey?: string;
  private wallet?: Wallet;

  constructor(baseUrl: string, opts?: string | { apiKey?: string; wallet?: Wallet }) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    if (typeof opts === "string") {
      // Legacy: ArknetClient(baseUrl, apiKey)
      this.apiKey = opts;
    } else {
      this.apiKey = opts?.apiKey;
      this.wallet = opts?.wallet;
    }
    this.apiKey ??= typeof process !== "undefined" ? process.env.ARKNET_WALLET : undefined;
  }

  /**
   * Auto-discover a gateway from the on-chain registry.
   * Contacts seed URLs, reads /v1/gateways, picks the best one.
   */
  static async connect(opts?: {
    seeds?: string[];
    requireHttps?: boolean;
    apiKey?: string;
    wallet?: Wallet;
  }): Promise<ArknetClient> {
    const seeds = opts?.seeds ?? (await fetchSeeds());
    for (const seed of seeds) {
      try {
        const resp = await fetch(`${seed.replace(/\/+$/, "")}/v1/gateways`);
        if (!resp.ok) continue;
        const data = await resp.json();
        const gateways = (data.gateways ?? []) as Array<{
          url: string;
          https: boolean;
        }>;
        gateways.sort((a, b) => (b.https ? 1 : 0) - (a.https ? 1 : 0));
        for (const gw of gateways) {
          if (opts?.requireHttps && !gw.https) continue;
          return new ArknetClient(gw.url, {
            apiKey: opts?.apiKey,
            wallet: opts?.wallet,
          });
        }
      } catch {
        continue;
      }
    }
    throw new Error("no reachable gateway found");
  }

  /** The wallet associated with this client, if any. */
  getWallet(): Wallet | undefined {
    return this.wallet;
  }

  private headers(): Record<string, string> {
    const h: Record<string, string> = { "Content-Type": "application/json" };
    if (this.apiKey) {
      h["Authorization"] = `Bearer ${this.apiKey}`;
    }
    return h;
  }

  /** Non-streaming chat completion. */
  async chatCompletion(
    req: ChatCompletionRequest
  ): Promise<ChatCompletionResponse> {
    const resp = await fetch(`${this.baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: this.headers(),
      body: JSON.stringify({ ...req, stream: false }),
    });
    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`arknet API error (${resp.status}): ${text}`);
    }
    return resp.json();
  }

  /** Streaming chat completion — returns an async iterator of chunks. */
  async *chatCompletionStream(
    req: Omit<ChatCompletionRequest, "stream">
  ): AsyncGenerator<ChatCompletionChunk> {
    const resp = await fetch(`${this.baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: this.headers(),
      body: JSON.stringify({ ...req, stream: true }),
    });
    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`arknet API error (${resp.status}): ${text}`);
    }
    const reader = resp.body?.getReader();
    if (!reader) throw new Error("no response body");

    const decoder = new TextDecoder();
    let buffer = "";

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;

      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() || "";

      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed.startsWith("data:")) continue;
        const payload = trimmed.slice(5).trim();
        if (payload === "[DONE]") return;
        yield JSON.parse(payload);
      }
    }
  }

  /** List registered models. */
  async listModels(): Promise<ModelsResponse> {
    const resp = await fetch(`${this.baseUrl}/v1/models`, {
      method: "GET",
      headers: this.headers(),
    });
    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`arknet API error (${resp.status}): ${text}`);
    }
    return resp.json();
  }
}
