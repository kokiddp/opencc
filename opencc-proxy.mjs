#!/usr/bin/env node
/**
 * opencc-proxy — traduttore Anthropic Messages API → OpenAI Responses API.
 *
 * Claude Code parla solo il protocollo Anthropic (/v1/messages); i backend
 * OpenAI parlano il protocollo Responses. Questo proxy locale traduce le
 * richieste di Claude Code verso OpenAI in due modalità:
 *
 *   subscription  (predefinita) → backend ChatGPT/Codex del piano ChatGPT
 *                                  (Plus/Pro/Team). Autenticazione OAuth letta
 *                                  da ~/.codex/auth.json (login `codex`).
 *   apikey                       → api.openai.com/v1 con OPENAI_API_KEY.
 *   go                           → pass-through Anthropic verso opencode-go;
 *                                  normalizza solo modello ed effort.
 *
 * La logica di traduzione è adattata dal proxy MIT-licensed di
 * codex-for-claude-code (https://github.com/Yusang-park/codex-for-claude-code).
 *
 * Uso (standalone):
 *   OPENCC_MODE=subscription node opencc-proxy.mjs
 *
 * Variabili d'ambiente:
 *   OPENCC_MODE            subscription (default) | apikey | go
 *   OPENCC_PROXY_PORT      porta di ascolto (default: 3199)
 *   OPENAI_API_KEY         chiave API OpenAI (solo modalità apikey)
 *   OPENAI_API_BASE        upstream API (default: https://api.openai.com/v1)
 *   CHATGPT_API_BASE       upstream abbonamento (default: https://chatgpt.com/backend-api/codex)
 *   OPENCC_FALLBACK_MODEL  modello OpenAI usato quando Claude Code richiede claude-*
 *   OPENCC_MODELS          elenco modelli (CSV) esposti da GET /v1/models
 *   OPENCC_EFFORT_POLICY_FILE  JSON con effort supportati/default per modello
 *   OPENCC_GO_BASE_URL     upstream Anthropic opencode-go (solo modalità go)
 *   OPENCODE_API_KEY       chiave x-api-key upstream (solo modalità go)
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

// Client OAuth del CLI Codex: il refresh_token emesso dal device flow è legato
// a questo client. Se possibile, il client_id viene letto dal claim del token.
const CODEX_CLIENT_ID = 'app_EMoamEEZ73f0CkXaXp7hrann';
const AUTH_ENDPOINT = (process.env.OPENAI_AUTH_BASE ?? 'https://auth.openai.com/oauth/token');

// ID claude-* che Claude Code potrebbe usare per i probe interni: li rimappa
// sul modello scelto, altrimenti verrebbero inoltrati ad Anthropic e fallirebbero.
const CLAUDE_MODEL_RE = /^claude-|^(opus|sonnet|haiku)(-|$)/i;

// Finestre di contesto effettive (per il campo usage.context_window; il valore
// usato da Claude Code viene impostato dallo script opencc via env).
// Valore = max_context_window × effective_context_window_percent (95%) dalla
// models_cache del CLI Codex.
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
  // ignora l'eventuale suffisso @effort
  const id = (model ?? '').split('@')[0];
  return MODEL_CONTEXT_WINDOWS[id] ?? null;
}

/**
 * Converte l'usage della Responses API nel formato Anthropic: OpenAI include i
 * token in cache nel totale input_tokens e li scompone in
 * input_tokens_details.cached_tokens; Anthropic li vuole separati
 * (cache_read_input_tokens). Senza questa conversione /usage mostrerebbe input
 * gonfiato e zero in cache per il backend openai.
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

// ── Auth: chiave API oppure token OAuth del CLI Codex ─────────────────────────
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
    ? 'Token OAuth scaduto. Esegui `opencc login` (o `codex login --device-auth`).'
    : 'Imposta la variabile OPENAI_API_KEY.';
}

// ── Refresh OAuth (rinnovo invisibile del token, come fa il CLI Codex) ───────
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

// Rinnova access_token via refresh_token e riscrive ~/.codex/auth.json.
// Ritorna il nuovo access_token o null in caso di errore.
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

// ── Specifica modello: "gpt-5.6-sol@high" → { id, effort } ───────────────────
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
 * Applica la policy reale del modello all'effort globale inviato da Claude Code.
 * Il client non conosce le capability dei modelli custom e non può filtrare
 * /effort: valori non supportati vengono ridotti al massimo livello disponibile
 * non superiore a quello richiesto; se non esiste, al minimo disponibile.
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
      `[opencc] effort ${spec.id}: ${normalizedEffort.requested ?? '(nessuno)'} -> `
      + `${normalizedEffort.applied ?? '(rimosso)'} (${normalizedEffort.reason})`,
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

// ── Collegamento tra turni (previous_response_id + input delta) ───────────────
// Codex non rimanda mai l'intera storia: verifica che la nuova richiesta sia
// un'estensione della precedente e invia solo il delta con previous_response_id;
// il server ricollega il contesto e fattura la parte ripetuta a tariffa cache.
// Senza questo, ogni turno rimanda la storia completa via HTTP: se la cache
// automatica (TTL ~5 min) scade, l'intero contesto viene rifatturato ogni volta.
// Lo stato è chiavato per sessione+agente (x-claude-code-session-id e
// x-claude-code-agent-id, inviati da Claude Code su ogni richiesta).

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

/** Normalizza gli arguments di un function_call: il round-trip raw→oggetto→JSON
 *  è deterministico, quindi coincide con ciò che Claude Code rispedisce. */
export function normalizeArguments(raw) {
  try {
    return JSON.stringify(JSON.parse(raw ?? '{}'));
  } catch {
    return JSON.stringify({});
  }
}

/** Stato della conversazione dopo una risposta: input inviato, item di output
 *  canonici, proprietà della richiesta e id della risposta. */
function rememberConversation(key, state) {
  CONVERSATIONS.set(key, state);
}

function forgetConversation(key) {
  if (key) CONVERSATIONS.delete(key);
}

// ── Copia di intestazioni risposta (per il pass-through) ──────────────────────
function copyResponseHeaders(headers) {
  const result = {};
  for (const [name, value] of headers) {
    // content-encoding va tolto: fetch (undici) decompressa già il body, quindi
    // inoltrare l'header originale farebbe fallire la decompressione a Claude
    // Code (BrotliDecompressionError). Chiediamo comunque identity all'upstream.
    if (!['connection', 'content-length', 'transfer-encoding', 'content-encoding'].includes(name.toLowerCase())) {
      result[name] = value;
    }
  }
  return result;
}

async function pipeAnthropicGo(req, res, url, body, fetchImpl) {
  if (!OPENCODE_API_KEY) {
    res.writeHead(401, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: { type: 'authentication_error', message: 'OPENCODE_API_KEY mancante.' } }));
    return;
  }
  const requestedModel = body.model ?? '';
  const spec = resolveModelSpec(requestedModel);
  if (!spec) {
    res.writeHead(400, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: { type: 'invalid_request_error', message: `Modello '${requestedModel}' non gestito.` } }));
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
        // Niente compressione upstream: evita che il pass-through inoltri un
        // Content-Encoding su un body già decompresso da fetch.
        'Accept-Encoding': 'identity',
        ...(req.headers['anthropic-version'] ? { 'anthropic-version': req.headers['anthropic-version'] } : {}),
        ...(req.headers['anthropic-beta'] ? { 'anthropic-beta': req.headers['anthropic-beta'] } : {}),
      },
      body: JSON.stringify(normalizedBody),
    });
  } catch (err) {
    res.writeHead(502, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: { message: `Errore upstream OpenCode Go: ${err.message}` } }));
    return;
  }

  res.writeHead(upstreamRes.status, copyResponseHeaders(upstreamRes.headers));
  if (!upstreamRes.body) {
    res.end();
    return;
  }
  try {
    for await (const chunk of upstreamRes.body) res.write(chunk);
  } catch { /* il client o l'upstream ha chiuso lo stream */ }
  res.end();
}

// ── Conversione richiesta Anthropic → Responses API ──────────────────────────
/**
 * Costruisce gli item di input della Responses API a partire dai messaggi
 * Anthropic. Usata sia per la richiesta sia per il confronto di estensione del
 * collegamento tra turni: deve quindi produrre item canonici e deterministici.
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

  // Il backend ChatGPT richiede store=false e stream=true; l'API OpenAI
  // accetta gli stessi parametri. I client non-stream vengono gestiti
  // raccogliendo gli eventi SSE.
  const req = {
    model: body.model,
    input,
    store: false,
    stream: true,
  };

  req.instructions = instructions || 'You are a helpful assistant.';

  // /effort di Claude Code arriva come output_config.effort. Il suffisso storico
  // modello@effort resta supportato e ha precedenza per compatibilità.
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

  // I backend OpenAI rifiutano i parametri di limiti token: NON includere
  // max_output_tokens.
  return req;
}

// ── Conversione risposta Responses API → Anthropic ───────────────────────────
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
    // richieste claude-* → modello scelto (che può avere a sua volta @effort)
    return FALLBACK_MODEL ? parseModelSpec(FALLBACK_MODEL) : null;
  }
  return spec;
}

// ── Server HTTP ───────────────────────────────────────────────────────────────
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
          message: `Modello '${requestedModel}' non gestito: configura OPENCC_FALLBACK_MODEL con un modello OpenAI.`,
        },
      }));
      return;
    }

    const auth = resolveAuth();
    if (!auth) {
      res.writeHead(401, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        error: { type: 'authentication_error', message: `Autenticazione non trovata. ${authHint()}` },
      }));
      return;
    }

    const { normalizedBody, normalizedEffort } = normalizeMessagesBody(body, spec);
    // buildResponsesAPIRequest riceve l'effort separatamente per tradurlo in
    // reasoning.effort; rimuovilo dal body Anthropic normalizzato.
    if (normalizedBody.output_config) {
      normalizedBody.output_config = { ...normalizedBody.output_config };
      delete normalizedBody.output_config.effort;
      if (Object.keys(normalizedBody.output_config).length === 0) delete normalizedBody.output_config;
    }
    const responsesReq = buildResponsesAPIRequest(normalizedBody, normalizedEffort.applied);
    const isStream = body.stream === true;

    // Collegamento tra turni: se la richiesta è un'estensione della precedente
    // per la stessa sessione, inviamo solo il delta con previous_response_id
    // (come fa codex). In caso di errore upstream si ripiega sulla richiesta
    // completa senza collegamento.
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
      forgetConversation(key); // contesto cambiato (modello, system o tools)
    }
    if (linked) {
      console.error(`[opencc] delta ${key}: ${deltaInput.length} item inviati (baseline ${baseline.length})`);
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
      // Riprova senza collegamento: input completo e niente previous_response_id.
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
      // Token OAuth scaduto: prova a rinnovarlo col refresh_token e riprova.
      if (upstreamRes.status === 401 && MODE === 'subscription') {
        const fresh = await refreshAuth(readAuth());
        if (fresh) {
          upstreamRes = await doFetch(fresh, readAuth()?.tokens?.account_id ?? auth.accountId);
        }
      }
      // Il collegamento può fallire (es. risposta scaduta lato server): ritenta
      // con la richiesta completa e azzera lo stato della conversazione.
      if (linked && !upstreamRes.ok) {
        console.error(`[opencc] delta fallito (${upstreamRes.status}): ritenta senza collegamento`);
        const token = MODE === 'subscription' ? readAuth()?.tokens?.access_token ?? auth.token : auth.token;
        const accountId = readAuth()?.tokens?.account_id ?? auth.accountId;
        upstreamRes = await fullFetch(token, accountId);
        forgetConversation(key);
        linked = false;
      }
    } catch (err) {
      res.writeHead(502, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { message: `Errore upstream: ${err.message}` } }));
      return;
    }

    if (!upstreamRes.ok) {
      const errText = await upstreamRes.text();
      let message = errText;
      try {
        const j = JSON.parse(errText);
        if (j?.error?.message) message = j.error.message;
      } catch { /* testo grezzo */ }
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
        // Raccolta degli item di output in forma canonica, per il collegamento
        // tra turni (baseline del prossimo confronto di estensione).
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
              // /usage accumula input/cache dall'usage del message_delta finale:
              // serve l'usage completo, non solo output_tokens.
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

      // Ricorda la conversazione per il collegamento del prossimo turno, e
      // registra l'usage per diagnostica (verifica del risparmio cache).
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
      // Client non-stream: raccogli gli eventi SSE e costruisci una risposta Anthropic.
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
        res.end(JSON.stringify({ error: { message: 'Nessuna risposta ricevuta dal backend OpenAI' } }));
      }
    }
  });
}

export function startServer() {
  const server = createServer();

  server.listen(PORT, '127.0.0.1', () => {
    process.stderr.write(`[opencc-proxy] in ascolto su http://127.0.0.1:${PORT} (mode=${MODE})\n`);
    process.stderr.write(`[opencc-proxy] upstream=${API_BASE}\n`);
  });

  server.on('error', (err) => {
    if (err.code === 'EADDRINUSE') {
      // Già in esecuzione — esci in silenzio.
      process.exit(0);
    }
    process.stderr.write(`[opencc-proxy] errore: ${err.message}\n`);
    process.exit(1);
  });

  return server;
}

if (process.env.OPENCC_PROXY_TEST !== '1') {
  startServer();
}
