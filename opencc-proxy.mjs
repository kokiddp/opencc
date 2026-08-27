#!/usr/bin/env node
/**
 * opencc-proxy — Anthropic Messages API → OpenAI Responses API translator.
 *
 * Claude Code speaks only the Anthropic protocol (/v1/messages); the OpenAI
 * backends speak the Responses protocol. This local proxy translates Claude
 * Code's requests toward OpenAI in two modes:
 *
 *   subscription  (default) → ChatGPT/Codex backend of the ChatGPT plan
 *                              (Plus/Pro/Team). OAuth authentication read
 *                              from ~/.codex/auth.json (login `codex`).
 *   apikey                   → api.openai.com/v1 with OPENAI_API_KEY.
 *   go                       → Anthropic pass-through to opencode-go;
 *                              normalizes model and effort only.
 *
 * The translation logic is adapted from the MIT-licensed proxy of
 * codex-for-claude-code (https://github.com/Yusang-park/codex-for-claude-code).
 *
 * Usage (standalone):
 *   OPENCC_MODE=subscription node opencc-proxy.mjs
 *
 * Environment variables:
 *   OPENCC_MODE            subscription (default) | apikey | go
 *   OPENCC_PROXY_PORT      listening port (default: 3199)
 *   OPENAI_API_KEY         OpenAI API key (apikey mode only)
 *   OPENAI_API_BASE        API upstream (default: https://api.openai.com/v1)
 *   CHATGPT_API_BASE       subscription upstream (default: https://chatgpt.com/backend-api/codex)
 *   OPENCC_FALLBACK_MODEL  OpenAI model used when Claude Code requests claude-*
 *   OPENCC_MODELS          model list (CSV) exposed by GET /v1/models
 *   OPENCC_EFFORT_POLICY_FILE  JSON with supported/default effort per model
 *   OPENCC_GO_BASE_URL     opencode-go Anthropic upstream (go mode only)
 *   OPENCODE_API_KEY       upstream x-api-key (go mode only)
 */
import http from 'node:http';
import { existsSync, readFileSync, writeFileSync, renameSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

export const PROXY_VERSION = '5';

const MODE = process.env.OPENCC_MODE ?? 'subscription';
const PORT = parseInt(process.env.OPENCC_PROXY_PORT ?? '3199', 10);
const FALLBACK_MODEL = process.env.OPENCC_FALLBACK_MODEL ?? '';
const OPENAI_API_KEY = process.env.OPENAI_API_KEY ?? '';
const OPENCODE_API_KEY = process.env.OPENCODE_API_KEY ?? '';
const GO_BASE_URL = (process.env.OPENCC_GO_BASE_URL ?? 'https://opencode.ai/zen/go').replace(/\/+$/, '');
const EFFORT_POLICY_PATH = process.env.OPENCC_EFFORT_POLICY_FILE ?? '';
const MODELS = (process.env.OPENCC_MODELS ?? '')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean);

const CODEX_AUTH_PATH = join(homedir(), '.codex', 'auth.json');
const API_BASE = (MODE === 'go'
  ? GO_BASE_URL
  : MODE === 'apikey'
    ? (process.env.OPENAI_API_BASE ?? 'https://api.openai.com/v1')
    : (process.env.CHATGPT_API_BASE ?? 'https://chatgpt.com/backend-api/codex')
).replace(/\/+$/, '');

// Codex CLI OAuth client: the refresh_token issued by the device flow is bound
// to this client. When possible, the client_id is read from the token claim.
const CODEX_CLIENT_ID = 'app_EMoamEEZ73f0CkXaXp7hrann';
const AUTH_ENDPOINT = (process.env.OPENAI_AUTH_BASE ?? 'https://auth.openai.com/oauth/token');

// claude-* IDs that Claude Code might use for internal probes: remap them to
// the chosen model, otherwise they would be forwarded to Anthropic and fail.
const CLAUDE_MODEL_RE = /^claude-|^(opus|sonnet|haiku)(-|$)/i;

// Effective context windows (for the usage.context_window field; the value
// used by Claude Code is set by the opencc script via env).
// Value = max_context_window × effective_context_window_percent (95%) from the
// Codex CLI models_cache.
const MODEL_CONTEXT_WINDOWS = {
  'gpt-5.6-sol': 828400,
  'gpt-5.6-terra': 828400,
  'gpt-5.6-luna': 828400,
  'gpt-reserve': 828400,
  'gpt-5.5': 258400,
  'gpt-5.4': 950000,
  'gpt-5.4-mini': 258400,
};

function getContextWindowForModel(model) {
  // ignore any @effort suffix
  const id = (model ?? '').split('@')[0];
  return MODEL_CONTEXT_WINDOWS[id] ?? null;
}

/**
 * Converts the Responses API usage into the Anthropic format: OpenAI includes
 * the cached tokens in the input_tokens total and breaks them down in
 * input_tokens_details.cached_tokens; Anthropic wants them separate
 * (cache_read_input_tokens). Without this conversion /usage would show
 * inflated input and zero cache for the openai backend.
 */
export function extractUsage(usage = {}) {
  const cached = usage.input_tokens_details?.cached_tokens ?? 0;
  const inputTotal = usage.input_tokens ?? 0;
  return {
    input_tokens: Math.max(0, inputTotal - cached),
    output_tokens: usage.output_tokens ?? 0,
    cache_read_input_tokens: cached,
    cache_creation_input_tokens: usage.cache_creation_input_tokens ?? 0,
  };
}

function buildUsagePayload(model, usage = {}) {
  const contextWindow = getContextWindowForModel(model);
  const inputTokens = usage.input_tokens ?? 0;
  const outputTokens = usage.output_tokens ?? 0;
  const cacheCreationInputTokens = usage.cache_creation_input_tokens ?? 0;
  const cacheReadInputTokens = usage.cache_read_input_tokens ?? 0;
  const totalInputTokens = inputTokens + cacheCreationInputTokens + cacheReadInputTokens;
  const payload = {
    model,
    input_tokens: inputTokens,
    output_tokens: outputTokens,
    total_input_tokens: totalInputTokens,
    total_output_tokens: outputTokens,
    current_usage: {
      input_tokens: inputTokens,
      cache_creation_input_tokens: cacheCreationInputTokens,
      cache_read_input_tokens: cacheReadInputTokens,
    },
  };
  if (contextWindow) {
    payload.context_window = contextWindow;
    payload.context_window_size = contextWindow;
    payload.used_percentage = (totalInputTokens / contextWindow) * 100;
  }
  return payload;
}

// ── Auth: API key or Codex CLI OAuth token ───────────────────────────────────
export function resolveAuth() {
  if (MODE === 'apikey') {
    return OPENAI_API_KEY ? { token: OPENAI_API_KEY, accountId: null } : null;
  }
  try {
    if (existsSync(CODEX_AUTH_PATH)) {
      const auth = JSON.parse(readFileSync(CODEX_AUTH_PATH, 'utf8'));
      if (auth.tokens?.access_token) {
        return { token: auth.tokens.access_token, accountId: auth.tokens.account_id ?? null };
      }
    }
  } catch { /* fall through */ }
  return null;
}

function authHint() {
  return MODE === 'subscription'
    ? 'OAuth token expired. Run `opencc login` (or `codex login --device-auth`).'
    : 'Set the OPENAI_API_KEY environment variable.';
}

// ── OAuth refresh (silent token renewal, like the Codex CLI does) ─────────────
export function readAuth() {
  try {
    if (existsSync(CODEX_AUTH_PATH)) {
      return JSON.parse(readFileSync(CODEX_AUTH_PATH, 'utf8'));
    }
  } catch { /* fall through */ }
  return null;
}

export function writeAuth(auth) {
  const tmp = `${CODEX_AUTH_PATH}.tmp`;
  writeFileSync(tmp, JSON.stringify(auth, null, 2) + '\n', { mode: 0o600 });
  renameSync(tmp, CODEX_AUTH_PATH);
}

function clientIdFromJwt(token) {
  try {
    const payload = (token ?? '').split('.')[1];
    if (!payload) return null;
    const json = JSON.parse(Buffer.from(payload, 'base64url').toString('utf8'));
    return json.client_id || null;
  } catch { return null; }
}

// Renews the access_token via refresh_token and rewrites ~/.codex/auth.json.
// Returns the new access_token, or null on error.
export async function refreshAuth(auth) {
  const refreshToken = auth?.tokens?.refresh_token;
  if (!refreshToken) return null;
  const clientId = clientIdFromJwt(auth.tokens?.access_token) || CODEX_CLIENT_ID;
  try {
    const res = await fetch(AUTH_ENDPOINT, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        grant_type: 'refresh_token',
        refresh_token: refreshToken,
        client_id: clientId,
      }),
    });
    if (!res.ok) return null;
    const data = await res.json();
    if (!data.access_token) return null;
    const newAuth = {
      ...auth,
      last_refresh: new Date().toISOString(),
      tokens: {
        ...auth.tokens,
        access_token: data.access_token,
        refresh_token: data.refresh_token || auth.tokens.refresh_token,
        id_token: data.id_token || auth.tokens.id_token,
        account_id: data.account_id || auth.tokens.account_id,
      },
    };
    writeAuth(newAuth);
    return data.access_token;
  } catch { return null; }
}

// ── Model spec: "gpt-5.6-sol@high" → { id, effort } ──────────────────────────
function parseModelSpec(model) {
  if (!model) return { id: '', effort: null };
  const m = model.match(/^([^@]+)(?:@(.+))?$/);
  return { id: m[1], effort: m[2] || null };
}

const EFFORT_ORDER = ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'];

function readEffortPolicy(model) {
  if (!EFFORT_POLICY_PATH) return null;
  try {
    const data = JSON.parse(readFileSync(EFFORT_POLICY_PATH, 'utf8'));
    const policy = data.models?.[model];
    if (!policy || !Array.isArray(policy.supported)) return null;
    const supported = policy.supported.filter((v) => EFFORT_ORDER.includes(v));
    const defaultEffort = supported.includes(policy.default) ? policy.default : null;
    return { supported, default: defaultEffort };
  } catch {
    return null;
  }
}

/**
 * Applies the model's real policy to the global effort sent by Claude Code.
 * The client has no knowledge of custom-model capabilities and cannot filter
 * /effort: unsupported values are reduced to the highest available level not
 * exceeding the requested one; if none exists, to the lowest available.
 */
export function normalizeEffort(model, requestedEffort, policy = readEffortPolicy(model)) {
  if (!policy) {
    return { requested: requestedEffort || null, applied: requestedEffort || null, reason: 'no-policy' };
  }

  const supported = policy.supported;
  if (supported.length === 0) {
    return {
      requested: requestedEffort || null,
      applied: null,
      reason: requestedEffort ? 'unsupported-model' : 'no-effort',
    };
  }

  if (!requestedEffort) {
    return { requested: null, applied: policy.default || null, reason: 'default' };
  }
  if (supported.includes(requestedEffort)) {
    return { requested: requestedEffort, applied: requestedEffort, reason: 'exact' };
  }

  const requestedRank = EFFORT_ORDER.indexOf(requestedEffort);
  if (requestedRank < 0) {
    return {
      requested: requestedEffort,
      applied: policy.default || supported[0],
      reason: 'unknown-level',
    };
  }

  const ranked = supported
    .map((value) => ({ value, rank: EFFORT_ORDER.indexOf(value) }))
    .filter(({ rank }) => rank >= 0)
    .sort((a, b) => a.rank - b.rank);
  const below = ranked.filter(({ rank }) => rank <= requestedRank);
  const applied = below.length > 0 ? below.at(-1).value : ranked[0]?.value || null;
  return { requested: requestedEffort, applied, reason: 'clamped' };
}

function normalizeMessagesBody(body, spec) {
  const requestedEffort = spec.effort || body.output_config?.effort || null;
  const normalizedEffort = normalizeEffort(spec.id, requestedEffort);
  if (normalizedEffort.requested !== normalizedEffort.applied) {
    console.error(
      `[opencc] effort ${spec.id}: ${normalizedEffort.requested ?? '(none)'} -> `
      + `${normalizedEffort.applied ?? '(removed)'} (${normalizedEffort.reason})`,
    );
  }

  const normalizedBody = { ...body, model: spec.id };
  if (normalizedBody.output_config) {
    normalizedBody.output_config = { ...normalizedBody.output_config };
    delete normalizedBody.output_config.effort;
    if (Object.keys(normalizedBody.output_config).length === 0) {
      delete normalizedBody.output_config;
    }
  }
  if (normalizedEffort.applied) {
    normalizedBody.output_config = {
      ...(normalizedBody.output_config ?? {}),
      effort: normalizedEffort.applied,
    };
  }
  return { normalizedBody, normalizedEffort };
}

// ── Turn chaining (previous_response_id + input delta) ────────────────────────
// Codex never resends the full history: it checks that the new request is an
// extension of the previous one and sends only the delta with
// previous_response_id; the server reconnects the context and bills the
// repeated part at cache rates. Without this, every turn resends the full
// history over HTTP: when the automatic cache (TTL ~5 min) expires, the whole
// context is re-billed every time. State is keyed by session+agent
// (x-claude-code-session-id and x-claude-code-agent-id, sent by Claude Code on
// every request).

const CONVERSATIONS = new Map();

function sessionKey(headers) {
  const session = headers['x-claude-code-session-id'] ?? '';
  const agent = headers['x-claude-code-agent-id'] ?? '';
  return session ? `${session}|${agent}` : null;
}

function canonicalProps(body, spec) {
  return JSON.stringify({
    model: spec.id,
    instructions: typeof body.system === 'string'
      ? body.system
      : (body.system ?? []).filter((b) => b.type === 'text').map((b) => b.text).join(''),
    tools: (body.tools ?? []).map((t) => ({
      name: t.name,
      description: t.description ?? '',
      parameters: t.input_schema ?? { type: 'object', properties: {} },
    })),
  });
}

const itemKey = (item) => JSON.stringify(item);

export function isExtension(baseline, input) {
  if (input.length < baseline.length) return false;
  for (let i = 0; i < baseline.length; i += 1) {
    if (itemKey(baseline[i]) !== itemKey(input[i])) return false;
  }
  return true;
}

/** Normalizes the arguments of a function_call: the raw→object→JSON round-trip
 *  is deterministic, so it matches what Claude Code resends. */
export function normalizeArguments(raw) {
  try {
    return JSON.stringify(JSON.parse(raw ?? '{}'));
  } catch {
    return JSON.stringify({});
  }
}

/** Conversation state after a response: input sent, canonical output items,
 *  request properties and the response id. */
function rememberConversation(key, state) {
  CONVERSATIONS.set(key, state);
}

function forgetConversation(key) {
  if (key) CONVERSATIONS.delete(key);
}

// ── Response header copying (for the pass-through) ────────────────────────────
function copyResponseHeaders(headers) {
  const result = {};
  for (const [name, value] of headers) {
    // content-encoding must be stripped: fetch (undici) already decompresses
    // the body, so forwarding the original header would make the
    // decompression fail on Claude Code (BrotliDecompressionError). We ask
    // the upstream for identity anyway.
    if (!['connection', 'content-length', 'transfer-encoding', 'content-encoding'].includes(name.toLowerCase())) {
      result[name] = value;
    }
  }
  return result;
}

async function pipeAnthropicGo(req, res, url, body, fetchImpl) {
  if (!OPENCODE_API_KEY) {
    res.writeHead(401, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: { type: 'authentication_error', message: 'OPENCODE_API_KEY is missing.' } }));
    return;
  }
  const requestedModel = body.model ?? '';
  const spec = resolveModelSpec(requestedModel);
  if (!spec) {
    res.writeHead(400, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: { type: 'invalid_request_error', message: `Model '${requestedModel}' not handled.` } }));
    return;
  }
  const { normalizedBody } = normalizeMessagesBody(body, spec);
  let upstreamRes;
  try {
    upstreamRes = await fetchImpl(`${GO_BASE_URL}${url.pathname}${url.search}`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'x-api-key': OPENCODE_API_KEY,
        // No upstream compression: prevents the pass-through from forwarding
        // a Content-Encoding on a body already decompressed by fetch.
        'Accept-Encoding': 'identity',
        ...(req.headers['anthropic-version'] ? { 'anthropic-version': req.headers['anthropic-version'] } : {}),
        ...(req.headers['anthropic-beta'] ? { 'anthropic-beta': req.headers['anthropic-beta'] } : {}),
      },
      body: JSON.stringify(normalizedBody),
    });
  } catch (err) {
    res.writeHead(502, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: { message: `OpenCode Go upstream error: ${err.message}` } }));
    return;
  }

  res.writeHead(upstreamRes.status, copyResponseHeaders(upstreamRes.headers));
  if (!upstreamRes.body) {
    res.end();
    return;
  }
  try {
    for await (const chunk of upstreamRes.body) res.write(chunk);
  } catch { /* the client or the upstream closed the stream */ }
  res.end();
}

// ── Anthropic request → Responses API conversion ─────────────────────────────
/**
 * Builds the Responses API input items from the Anthropic messages. Used both
 * for the request and for the turn-chaining extension check: it must therefore
 * produce canonical, deterministic items.
 */
export function buildInputItems(messages) {
  const input = [];
  for (const msg of messages ?? []) {
    if (typeof msg.content === 'string') {
      input.push({ role: msg.role, content: msg.content });
      continue;
    }
    for (const block of msg.content ?? []) {
      if (block.type === 'text') {
        input.push({ role: msg.role, content: block.text });
      } else if (block.type === 'tool_use') {
        input.push({
          type: 'function_call',
          call_id: block.id,
          name: block.name,
          arguments: JSON.stringify(block.input ?? {}),
        });
      } else if (block.type === 'tool_result') {
        const content = typeof block.content === 'string'
          ? block.content
          : (Array.isArray(block.content)
            ? block.content.filter((b) => b.type === 'text').map((b) => b.text).join('')
            : '');
        input.push({
          type: 'function_call_output',
          call_id: block.tool_use_id,
          output: content,
        });
      }
    }
  }
  return input;
}

export function buildResponsesAPIRequest(body, modelEffort) {
  const input = buildInputItems(body.messages);

  let instructions;
  if (body.system) {
    instructions = typeof body.system === 'string'
      ? body.system
      : body.system.filter((b) => b.type === 'text').map((b) => b.text).join('');
  }

  // The ChatGPT backend requires store=false and stream=true; the OpenAI API
  // accepts the same parameters. Non-stream clients are handled by collecting
  // the SSE events.
  const req = {
    model: body.model,
    input,
    store: false,
    stream: true,
  };

  req.instructions = instructions || 'You are a helpful assistant.';

  // Claude Code's /effort arrives as output_config.effort. The historical
  // model@effort suffix stays supported and takes precedence for compatibility.
  const effort = modelEffort || body.output_config?.effort;
  if (effort) {
    req.reasoning = { effort };
  }

  if (body.tools?.length) {
    req.tools = body.tools.map((t) => ({
      type: 'function',
      name: t.name,
      description: t.description ?? '',
      parameters: t.input_schema ?? { type: 'object', properties: {} },
    }));
  }

  // The OpenAI backends reject token-limit parameters: do NOT include
  // max_output_tokens.
  return req;
}

// ── Responses API response → Anthropic ───────────────────────────────────────
function translateResponsesAPIResponse(resp, originalModel) {
  const content = [];
  const usage = extractUsage(resp.usage);

  for (const item of resp.output ?? []) {
    if (item.type === 'message') {
      for (const c of item.content ?? []) {
        if (c.type === 'output_text') {
          content.push({ type: 'text', text: c.text });
        }
      }
    } else if (item.type === 'function_call') {
      let input = {};
      try { input = JSON.parse(item.arguments ?? '{}'); } catch { /* keep empty */ }
      content.push({
        type: 'tool_use',
        id: item.call_id ?? `toolu_${Date.now()}`,
        name: item.name,
        input,
      });
    }
  }

  const hasToolUse = content.some((c) => c.type === 'tool_use');

  return {
    id: `msg_${resp.id ?? Date.now()}`,
    type: 'message',
    role: 'assistant',
    content: content.length > 0 ? content : [{ type: 'text', text: '' }],
    model: originalModel,
    stop_reason: hasToolUse ? 'tool_use' : 'end_turn',
    stop_sequence: null,
    usage: buildUsagePayload(originalModel, usage),
  };
}

function formatSSE(event, data) {
  return `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
}

function resolveModelSpec(model) {
  const spec = parseModelSpec(model ?? '');
  if (CLAUDE_MODEL_RE.test(spec.id)) {
    // claude-* requests → the chosen model (which may itself carry @effort)
    return FALLBACK_MODEL ? parseModelSpec(FALLBACK_MODEL) : null;
  }
  return spec;
}

// ── HTTP server ───────────────────────────────────────────────────────────────
export function createServer({ fetchImpl = fetch } = {}) {
  return http.createServer(async (req, res) => {
    const url = new URL(req.url ?? '/', `http://127.0.0.1:${PORT}`);

    if (req.method === 'GET' && (url.pathname === '/health' || url.pathname === '/healthz')) {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ ok: true, mode: MODE, port: PORT, version: PROXY_VERSION, fallback: FALLBACK_MODEL }));
      return;
    }

    if (req.method === 'GET' && (url.pathname === '/v1/models' || url.pathname === '/models')) {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        object: 'list',
        data: MODELS.map((id) => ({ id, object: 'model', owned_by: 'openai' })),
      }));
      return;
    }

    if (req.method !== 'POST' || !url.pathname.includes('/messages')) {
      res.writeHead(404);
      res.end('Not Found');
      return;
    }

    let rawBody = '';
    try {
      for await (const chunk of req) rawBody += chunk;
    } catch {
      res.writeHead(400);
      res.end('Bad Request');
      return;
    }

    let body;
    try {
      body = JSON.parse(rawBody);
    } catch {
      res.writeHead(400, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { type: 'invalid_request_error', message: 'Invalid JSON' } }));
      return;
    }

    if (MODE === 'go') {
      await pipeAnthropicGo(req, res, url, body, fetchImpl);
      return;
    }

    const requestedModel = body.model ?? '';
    const spec = resolveModelSpec(requestedModel);
    if (!spec) {
      res.writeHead(400, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        error: {
          type: 'invalid_request_error',
          message: `Model '${requestedModel}' not handled: set OPENCC_FALLBACK_MODEL to an OpenAI model.`,
        },
      }));
      return;
    }

    const auth = resolveAuth();
    if (!auth) {
      res.writeHead(401, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        error: { type: 'authentication_error', message: `Authentication not found. ${authHint()}` },
      }));
      return;
    }

    const { normalizedBody, normalizedEffort } = normalizeMessagesBody(body, spec);
    // buildResponsesAPIRequest receives the effort separately to translate it
    // into reasoning.effort; remove it from the normalized Anthropic body.
    if (normalizedBody.output_config) {
      normalizedBody.output_config = { ...normalizedBody.output_config };
      delete normalizedBody.output_config.effort;
      if (Object.keys(normalizedBody.output_config).length === 0) delete normalizedBody.output_config;
    }
    const responsesReq = buildResponsesAPIRequest(normalizedBody, normalizedEffort.applied);
    const isStream = body.stream === true;

    // Turn chaining: if the request is an extension of the previous one for
    // the same session, we send only the delta with previous_response_id
    // (like codex does). On upstream error we fall back to the full request
    // without chaining.
    const input = responsesReq.input;
    const props = canonicalProps(normalizedBody, spec);
    const key = isStream ? sessionKey(req.headers) : null;
    const conv = key ? CONVERSATIONS.get(key) : null;
    let linked = false;
    let deltaInput = null;
    let baseline = [];
    if (conv && conv.lastResponseId && conv.props === props) {
      baseline = conv.lastInput.concat(conv.lastResponseItems);
      if (isExtension(baseline, input)) {
        deltaInput = input.slice(baseline.length);
        if (deltaInput.length > 0) {
          linked = true;
          responsesReq.previous_response_id = conv.lastResponseId;
          responsesReq.input = deltaInput;
        }
      }
    }
    if (!linked && key && conv && conv.props !== props) {
      forgetConversation(key); // context changed (model, system or tools)
    }
    if (linked) {
      console.error(`[opencc] delta ${key}: ${deltaInput.length} items sent (baseline ${baseline.length})`);
    }

    const doFetch = (token, accountId) => fetchImpl(`${API_BASE}/responses`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Authorization': `Bearer ${token}`,
        ...(accountId ? { 'ChatGPT-Account-ID': accountId } : {}),
      },
      body: JSON.stringify(responsesReq),
    });

    const fullFetch = (token, accountId) => {
      // Retry without chaining: full input and no previous_response_id.
      const fullReq = { ...responsesReq };
      delete fullReq.previous_response_id;
      fullReq.input = input;
      return fetchImpl(`${API_BASE}/responses`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'Authorization': `Bearer ${token}`,
          ...(accountId ? { 'ChatGPT-Account-ID': accountId } : {}),
        },
        body: JSON.stringify(fullReq),
      });
    };

    let upstreamRes;
    try {
      upstreamRes = await doFetch(auth.token, auth.accountId);
      // Expired OAuth token: try renewing it with the refresh_token and retry.
      if (upstreamRes.status === 401 && MODE === 'subscription') {
        const fresh = await refreshAuth(readAuth());
        if (fresh) {
          upstreamRes = await doFetch(fresh, readAuth()?.tokens?.account_id ?? auth.accountId);
        }
      }
      // Chaining can fail (e.g. response expired server-side): retry with the
      // full request and reset the conversation state.
      if (linked && !upstreamRes.ok) {
        console.error(`[opencc] delta failed (${upstreamRes.status}): retrying without chaining`);
        const token = MODE === 'subscription' ? readAuth()?.tokens?.access_token ?? auth.token : auth.token;
        const accountId = readAuth()?.tokens?.account_id ?? auth.accountId;
        upstreamRes = await fullFetch(token, accountId);
        forgetConversation(key);
        linked = false;
      }
    } catch (err) {
      res.writeHead(502, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { message: `Upstream error: ${err.message}` } }));
      return;
    }

    if (!upstreamRes.ok) {
      const errText = await upstreamRes.text();
      let message = errText;
      try {
        const j = JSON.parse(errText);
        if (j?.error?.message) message = j.error.message;
      } catch { /* raw text */ }
      if (upstreamRes.status === 401) {
        message = `${message} ${authHint()}`;
      }
      res.writeHead(upstreamRes.status, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { message } }));
      return;
    }

    const originalModel = body.model;
    let lineBuffer = '';
    let finalResponse = null;
    let streamUsage = null;

    if (isStream) {
      res.writeHead(200, {
        'Content-Type': 'text/event-stream',
        'Cache-Control': 'no-cache',
        'Connection': 'keep-alive',
      });

      const state = {
        started: false,
        msgId: `msg_${Date.now()}`,
        nextBlockIdx: 0,
        blocks: new Map(),
        hasToolUse: false,
        // Collection of the output items in canonical form, for the turn
        // chaining (baseline of the next extension check).
        respTexts: new Map(),
        respToolCalls: new Map(),
        respId: null,
      };

      const ensureMessageStart = () => {
        if (state.started) return;
        state.started = true;
        res.write(formatSSE('message_start', {
          type: 'message_start',
          message: {
            id: state.msgId,
            type: 'message',
            role: 'assistant',
            content: [],
            model: originalModel,
            stop_reason: null,
            stop_sequence: null,
            usage: buildUsagePayload(originalModel),
          },
        }));
        res.write(formatSSE('ping', { type: 'ping' }));
      };

      const openTextBlock = (outputIndex) => {
        ensureMessageStart();
        const idx = state.nextBlockIdx++;
        state.blocks.set(outputIndex, { idx, type: 'text', open: true });
        res.write(formatSSE('content_block_start', { type: 'content_block_start', index: idx, content_block: { type: 'text', text: '' } }));
        return idx;
      };

      const openToolBlock = (outputIndex, item) => {
        ensureMessageStart();
        const idx = state.nextBlockIdx++;
        state.blocks.set(outputIndex, { idx, type: 'tool_use', open: true });
        state.hasToolUse = true;
        res.write(formatSSE('content_block_start', {
          type: 'content_block_start',
          index: idx,
          content_block: {
            type: 'tool_use',
            id: item.call_id ?? item.id ?? `toolu_${Date.now()}_${idx}`,
            name: item.name ?? '',
            input: {},
          },
        }));
        return idx;
      };

      const closeBlock = (outputIndex) => {
        const b = state.blocks.get(outputIndex);
        if (!b || !b.open) return;
        b.open = false;
        res.write(formatSSE('content_block_stop', { type: 'content_block_stop', index: b.idx }));
      };

      try {
        for await (const rawChunk of upstreamRes.body) {
          lineBuffer += new TextDecoder().decode(rawChunk);
          const lines = lineBuffer.split('\n');
          lineBuffer = lines.pop() ?? '';

          for (const line of lines) {
            if (!line.startsWith('data: ')) continue;
            const payload = line.slice(6).trim();
            if (!payload || payload === '[DONE]') continue;

            let evt;
            try { evt = JSON.parse(payload); } catch { continue; }

            if (evt.type === 'response.completed' || evt.type === 'response.done') {
              finalResponse = evt.response ?? evt;
              // /usage accumulates input/cache from the final message_delta
              // usage: the full usage is needed, not just output_tokens.
              streamUsage = extractUsage(finalResponse.usage);
              state.respId = finalResponse.id ?? null;
              continue;
            }

            if (evt.type === 'response.output_item.added' && evt.item) {
              const oi = evt.output_index ?? evt.item.index ?? state.blocks.size;
              if (evt.item.type === 'function_call') {
                openToolBlock(oi, evt.item);
                state.respToolCalls.set(oi, {
                  call_id: evt.item.call_id ?? evt.item.id ?? '',
                  name: evt.item.name ?? '',
                  args: '',
                });
              }
              continue;
            }

            if (evt.type === 'response.output_item.done') {
              const oi = evt.output_index ?? evt.item?.index;
              if (oi !== undefined) closeBlock(oi);
              continue;
            }

            if (evt.type === 'response.output_text.delta' && evt.delta) {
              const oi = evt.output_index ?? 0;
              state.respTexts.set(oi, (state.respTexts.get(oi) ?? '') + evt.delta);
              let b = state.blocks.get(oi);
              if (!b) {
                openTextBlock(oi);
                b = state.blocks.get(oi);
              }
              res.write(formatSSE('content_block_delta', { type: 'content_block_delta', index: b.idx, delta: { type: 'text_delta', text: evt.delta } }));
              continue;
            }

            if (evt.type === 'response.function_call_arguments.delta' && evt.delta) {
              const oi = evt.output_index;
              const tool = state.respToolCalls.get(oi);
              if (tool) tool.args += evt.delta;
              const b = state.blocks.get(oi);
              if (!b) continue;
              res.write(formatSSE('content_block_delta', { type: 'content_block_delta', index: b.idx, delta: { type: 'input_json_delta', partial_json: evt.delta } }));
              continue;
            }

            if (evt.type === 'response.function_call_arguments.done') {
              const oi = evt.output_index;
              closeBlock(oi);
              continue;
            }
          }
        }
      } catch { /* best-effort */ }

      for (const [oi] of state.blocks) closeBlock(oi);

      // Remember the conversation for the next turn's chaining, and record
      // the usage for diagnostics (verifying the cache savings).
      const responseItems = [];
      const allIdx = new Set([...state.respTexts.keys(), ...state.respToolCalls.keys()]);
      for (const oi of [...allIdx].sort((a, b) => a - b)) {
        if (state.respTexts.has(oi)) {
          responseItems.push({ role: 'assistant', content: state.respTexts.get(oi) });
        } else {
          const tool = state.respToolCalls.get(oi);
          responseItems.push({
            type: 'function_call',
            call_id: tool.call_id,
            name: tool.name,
            arguments: normalizeArguments(tool.args),
          });
        }
      }
      if (key && state.respId) {
        rememberConversation(key, {
          lastResponseId: state.respId,
          lastInput: input,
          lastResponseItems: responseItems,
          props,
        });
      }
      const su = streamUsage ?? {};
      console.error(
        `[opencc] usage ${spec.id}: in=${su.input_tokens ?? 0} cached=${su.cache_read_input_tokens ?? 0} `
        + `out=${su.output_tokens ?? 0} (${linked ? 'delta' : 'full'})`,
      );

      if (state.started) {
        res.write(formatSSE('message_delta', {
          type: 'message_delta',
          delta: { stop_reason: state.hasToolUse ? 'tool_use' : 'end_turn', stop_sequence: null },
          usage: buildUsagePayload(originalModel, streamUsage ?? { output_tokens: 0 }),
        }));
        res.write(formatSSE('message_stop', { type: 'message_stop' }));
      } else {
        res.write(formatSSE('message_start', {
          type: 'message_start',
          message: {
            id: state.msgId,
            type: 'message',
            role: 'assistant',
            content: [],
            model: originalModel,
            stop_reason: null,
            stop_sequence: null,
            usage: buildUsagePayload(originalModel),
          },
        }));
        res.write(formatSSE('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } }));
        res.write(formatSSE('content_block_stop', { type: 'content_block_stop', index: 0 }));
        res.write(formatSSE('message_delta', {
          type: 'message_delta',
          delta: { stop_reason: 'end_turn', stop_sequence: null },
          usage: buildUsagePayload(originalModel),
        }));
        res.write(formatSSE('message_stop', { type: 'message_stop' }));
      }
      res.end();
    } else {
      // Non-stream client: collect the SSE events and build an Anthropic response.
      let collectedText = '';
      const collectedToolCalls = [];
      let collectedUsage = { input_tokens: 0, output_tokens: 0 };

      try {
        for await (const rawChunk of upstreamRes.body) {
          lineBuffer += new TextDecoder().decode(rawChunk);
          const lines = lineBuffer.split('\n');
          lineBuffer = lines.pop() ?? '';

          for (const line of lines) {
            if (!line.startsWith('data: ')) continue;
            const payload = line.slice(6).trim();
            if (!payload || payload === '[DONE]') continue;
            let evt;
            try { evt = JSON.parse(payload); } catch { continue; }

            if (evt.type === 'response.output_text.delta' && evt.delta) {
              collectedText += evt.delta;
            } else if (evt.type === 'response.function_call_arguments.done') {
              let input = {};
              try { input = JSON.parse(evt.arguments ?? '{}'); } catch { /* keep empty */ }
              collectedToolCalls.push({
                type: 'tool_use',
                id: evt.call_id ?? `toolu_${Date.now()}`,
                name: evt.name,
                input,
              });
            } else if (evt.type === 'response.completed' || evt.type === 'response.done') {
              const r = evt.response ?? evt;
              collectedUsage = extractUsage(r.usage);
            }
          }
        }
      } catch { /* best-effort */ }

      const content = [];
      if (collectedText) content.push({ type: 'text', text: collectedText });
      for (const tc of collectedToolCalls) content.push(tc);
      const hasToolUse = collectedToolCalls.length > 0;

      if (content.length > 0 || collectedUsage.output_tokens > 0) {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({
          id: `msg_${Date.now()}`,
          type: 'message',
          role: 'assistant',
          content: content.length > 0 ? content : [{ type: 'text', text: '' }],
          model: originalModel,
          stop_reason: hasToolUse ? 'tool_use' : 'end_turn',
          stop_sequence: null,
          usage: buildUsagePayload(originalModel, collectedUsage),
        }));
      } else {
        res.writeHead(502, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ error: { message: 'No response received from the OpenAI backend' } }));
      }
    }
  });
}

export function startServer() {
  const server = createServer();

  server.listen(PORT, '127.0.0.1', () => {
    process.stderr.write(`[opencc-proxy] listening on http://127.0.0.1:${PORT} (mode=${MODE})\n`);
    process.stderr.write(`[opencc-proxy] upstream=${API_BASE}\n`);
  });

  server.on('error', (err) => {
    if (err.code === 'EADDRINUSE') {
      // Already running — exit silently.
      process.exit(0);
    }
    process.stderr.write(`[opencc-proxy] error: ${err.message}\n`);
    process.exit(1);
  });

  return server;
}

if (process.env.OPENCC_PROXY_TEST !== '1') {
  startServer();
}
