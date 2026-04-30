/**
 * @arknet/sdk — TypeScript SDK for the arknet decentralized AI inference network.
 *
 * OpenAI-compatible client: point `baseUrl` at an arknet node and use
 * the same API shape as the OpenAI TypeScript SDK.
 *
 * @example
 * ```ts
 * import { ArknetClient } from "@arknet/sdk";
 *
 * const client = new ArknetClient("http://127.0.0.1:3000");
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

export class ArknetClient {
  private baseUrl: string;
  private apiKey?: string;

  constructor(baseUrl: string, apiKey?: string) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.apiKey = apiKey;
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
