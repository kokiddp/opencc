import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';

process.env.OPENCC_PROXY_TEST = '1';
process.env.OPENCC_MODE = 'apikey';
process.env.OPENAI_API_KEY = 'test-key';
process.env.OPENCC_FALLBACK_MODEL = 'gpt-fallback@high';
process.env.OPENCC_MODELS = 'gpt-one,gpt-two';
const policyDir = mkdtempSync(join(tmpdir(), 'opencc-test-'));
const policyFile = join(policyDir, 'model-efforts.json');
writeFileSync(policyFile, JSON.stringify({
  models: {
    'gpt-one': { supported: [], default: null },
    'gpt-two': { supported: ['low', 'medium', 'high'], default: 'medium' },
  },
}));
process.env.OPENCC_EFFORT_POLICY_FILE = policyFile;
process.on('exit', () => rmSync(policyDir, { recursive: true, force: true }));

const { buildResponsesAPIRequest, createServer, normalizeEffort, extractUsage, isExtension, normalizeArguments } = await import('./opencc-proxy.mjs');

test('traduce output_config.effort nel formato Responses', () => {
  const req = buildResponsesAPIRequest({
    model: 'gpt-one',
    messages: [{ role: 'user', content: 'ciao' }],
    output_config: { effort: 'xhigh' },
  }, null);

  assert.equal(req.model, 'gpt-one');
  assert.deepEqual(req.reasoning, { effort: 'xhigh' });
});

test('il suffisso @effort ha precedenza per compatibilità', () => {
  const req = buildResponsesAPIRequest({
    model: 'gpt-one',
    messages: [],
    output_config: { effort: 'low' },
  }, 'max');

  assert.deepEqual(req.reasoning, { effort: 'max' });
});

test('normalizza l’effort in base alle capability reali del modello', () => {
  const sparse = { supported: ['low', 'high', 'max'], default: 'high' };
  assert.deepEqual(normalizeEffort('gpt-one', 'high', sparse), {
    requested: 'high', applied: 'high', reason: 'exact',
  });
  assert.deepEqual(normalizeEffort('gpt-one', 'medium', sparse), {
    requested: 'medium', applied: 'low', reason: 'clamped',
  });
  assert.deepEqual(normalizeEffort('gpt-one', 'xhigh', sparse), {
    requested: 'xhigh', applied: 'high', reason: 'clamped',
  });
  assert.deepEqual(normalizeEffort('gpt-one', null, sparse), {
    requested: null, applied: 'high', reason: 'default',
  });
});

test('riconosce l’estensione della conversazione per il collegamento turni', () => {
  const base = [{ role: 'user', content: 'ciao' }];
  const full = [
    { role: 'user', content: 'ciao' },
    { role: 'assistant', content: 'ok' },
    { role: 'user', content: 'e poi?' },
  ];
  assert.equal(isExtension(base, full), true);
  assert.equal(isExtension(base, base), true);
  assert.equal(isExtension(full, base), false);
  assert.equal(isExtension([{ role: 'user', content: 'diverso' }], full), false);
  assert.equal(normalizeArguments('{"a":1,"b":[2]}'), '{"a":1,"b":[2]}');
  assert.equal(normalizeArguments('non-json'), '{}');
});

test('converte l’usage Responses nel formato Anthropic per /usage', () => {
  assert.deepEqual(extractUsage({
    input_tokens: 120,
    output_tokens: 40,
    input_tokens_details: { cached_tokens: 20 },
    output_tokens_details: { reasoning_tokens: 5 },
  }), {
    input_tokens: 100,
    output_tokens: 40,
    cache_read_input_tokens: 20,
    cache_creation_input_tokens: 0,
  });
  assert.deepEqual(extractUsage(undefined), {
    input_tokens: 0,
    output_tokens: 0,
    cache_read_input_tokens: 0,
    cache_creation_input_tokens: 0,
  });
});

test('rimuove l’effort per un modello che non lo espone', () => {
  assert.deepEqual(normalizeEffort('gpt-one', 'max', { supported: [], default: null }), {
    requested: 'max', applied: null, reason: 'unsupported-model',
  });
});

test('espone i modelli e applica la policy all’effort scelto in sessione', async (t) => {
  const upstreamBodies = [];
  const fetchImpl = async (_url, options) => {
    upstreamBodies.push(JSON.parse(options.body));
    return new Response([
      'data: {"type":"response.output_text.delta","delta":"ok"}',
      'data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}',
      '',
    ].join('\n'), { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
  };

  const server = createServer({ fetchImpl });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => server.close());
  const { port } = server.address();

  const models = await fetch(`http://127.0.0.1:${port}/v1/models`).then((r) => r.json());
  assert.deepEqual(models.data.map((m) => m.id), ['gpt-one', 'gpt-two']);

  const response = await fetch(`http://127.0.0.1:${port}/v1/messages`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      model: 'gpt-two',
      messages: [{ role: 'user', content: 'ciao' }],
      output_config: { effort: 'medium' },
      stream: false,
    }),
  });

  assert.equal(response.status, 200);
  assert.equal(upstreamBodies[0].model, 'gpt-two');
  assert.deepEqual(upstreamBodies[0].reasoning, { effort: 'medium' });

  const unsupported = await fetch(`http://127.0.0.1:${port}/v1/messages`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      model: 'gpt-one',
      messages: [{ role: 'user', content: 'ciao' }],
      output_config: { effort: 'max' },
      stream: false,
    }),
  });
  assert.equal(unsupported.status, 200);
  assert.equal(upstreamBodies[1].model, 'gpt-one');
  assert.equal(upstreamBodies[1].reasoning, undefined);
});

test('pass-through go normalizza effort e conserva la risposta Anthropic', async (t) => {
  let upstreamBody;
  let upstreamHeaders;
  const upstream = (await import('node:http')).default.createServer(async (req, res) => {
    let raw = '';
    for await (const chunk of req) raw += chunk;
    upstreamBody = JSON.parse(raw);
    upstreamHeaders = req.headers;
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({
      id: 'msg_go', type: 'message', role: 'assistant', model: upstreamBody.model,
      content: [{ type: 'text', text: 'ok' }], stop_reason: 'end_turn',
      usage: { input_tokens: 1, output_tokens: 1 },
    }));
  });
  await new Promise((resolve) => upstream.listen(0, '127.0.0.1', resolve));
  t.after(() => upstream.close());
  const upstreamPort = upstream.address().port;

  const proxyPort = upstreamPort + 1;
  const proxy = spawn(process.execPath, ['./opencc-proxy.mjs'], {
    cwd: import.meta.dirname,
    env: {
      ...process.env,
      OPENCC_PROXY_TEST: '0',
      OPENCC_MODE: 'go',
      OPENCC_PROXY_PORT: String(proxyPort),
      OPENCC_GO_BASE_URL: `http://127.0.0.1:${upstreamPort}`,
      OPENCODE_API_KEY: 'go-test-key',
      OPENCC_EFFORT_POLICY_FILE: policyFile,
    },
    stdio: ['ignore', 'ignore', 'ignore'],
  });
  t.after(() => proxy.kill());
  for (let i = 0; i < 50; i += 1) {
    try {
      const health = await fetch(`http://127.0.0.1:${proxyPort}/health`);
      if (health.ok) break;
    } catch { /* attende l'avvio */ }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }

  const response = await fetch(`http://127.0.0.1:${proxyPort}/v1/messages?beta=true`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'anthropic-version': '2023-06-01',
      'anthropic-beta': 'test-beta',
    },
    body: JSON.stringify({
      model: 'gpt-two',
      messages: [{ role: 'user', content: 'ciao' }],
      output_config: { effort: 'max', format: { type: 'json_schema' } },
      stream: false,
    }),
  });

  assert.equal(response.status, 200);
  assert.equal(upstreamBody.model, 'gpt-two');
  assert.deepEqual(upstreamBody.output_config, {
    format: { type: 'json_schema' }, effort: 'high',
  });
  assert.equal(upstreamHeaders['x-api-key'], 'go-test-key');
  assert.equal(upstreamHeaders['anthropic-version'], '2023-06-01');
  assert.equal(upstreamHeaders['anthropic-beta'], 'test-beta');
  assert.equal((await response.json()).id, 'msg_go');
});
