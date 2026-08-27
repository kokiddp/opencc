//! The local proxy: Anthropic Messages API → OpenAI Responses API translator.
//!
//! Claude Code speaks only the Anthropic protocol (`/v1/messages`); the OpenAI
//! backends speak the Responses protocol. This local proxy translates Claude
//! Code's requests in three modes:
//!
//! - `subscription` (default) → the ChatGPT/Codex backend of the ChatGPT plan,
//!   authenticating with the OAuth token in `~/.codex/auth.json` (refreshed
//!   silently via `refresh_token`).
//! - `apikey` → `api.openai.com/v1` with `OPENAI_API_KEY`.
//! - `opencode` → Anthropic pass-through to the opencode-go gateway;
//!   normalizes model and effort only.
//!
//! Direct port of `opencc-proxy.mjs` (itself adapted from the MIT-licensed
//! proxy of codex-for-claude-code).

use crate::effort::{
    normalize_effort, parse_model_spec, read_effort_policy, EffortDecision, ModelSpec,
};
use crate::state;
use crate::util::jwt_payload;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Channel};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The response body type used everywhere: a buffered channel written by the
/// handler (hyper 1.x has no boxed `Body` type anymore; the old
/// `hyper::body::Body::channel` moved to `http-body-util`).
type ResBody = Channel<Bytes, Infallible>;

/// Version reported by `/health`; the wrapper accepts only a proxy running
/// the same version and mode.
pub const PROXY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Codex CLI OAuth client: the refresh_token issued by the device flow is
/// bound to this client. When possible, the client_id is read from the token
/// claim.
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// claude-* IDs that Claude Code might use for internal probes: remap them to
/// the chosen model, otherwise they would be forwarded to Anthropic and fail.
/// Same as the node regex /^claude-|^(opus|sonnet|haiku)(-|$)/i.
fn is_claude_model(id: &str) -> bool {
    id.starts_with("claude-")
        || ["opus", "sonnet", "haiku"].iter().any(|alias| {
            id.strip_prefix(alias)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
}

/// Effective context windows (for the `usage.context_window` field; the value
/// used by Claude Code is set by the wrapper via env).
/// Value = max_context_window × effective_context_window_percent (95%).
const MODEL_CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("gpt-5.6-sol", 828400),
    ("gpt-5.6-terra", 828400),
    ("gpt-5.6-luna", 828400),
    ("gpt-reserve", 828400),
    ("gpt-5.5", 258400),
    ("gpt-5.4", 950000),
    ("gpt-5.4-mini", 258400),
];

pub struct Config {
    pub mode: String,
    pub port: u16,
    pub fallback_model: String,
    pub openai_api_key: String,
    pub opencode_api_key: String,
    /// opencode-go Anthropic upstream (opencode mode only).
    pub go_base_url: String,
    pub effort_policy_path: Option<PathBuf>,
    /// Model list exposed by GET /v1/models (CSV from the wrapper).
    pub models: Vec<String>,
    pub codex_auth_path: PathBuf,
    /// Upstream base URL for the Responses API (per mode).
    pub api_base: String,
    /// OAuth token endpoint (overridable for tests).
    pub auth_endpoint: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Config {
        let mode = env_or("OPENCC_MODE", "subscription");
        let port = env_or("OPENCC_PROXY_PORT", "3199")
            .parse::<u16>()
            .unwrap_or(3199);
        let go_base_url = env_or("OPENCC_GO_BASE_URL", "https://opencode.ai/zen/go");
        let go_base_url = go_base_url.trim_end_matches('/').to_string();
        let api_base = match mode.as_str() {
            "opencode" => go_base_url.clone(),
            "apikey" => env_or("OPENAI_API_BASE", "https://api.openai.com/v1"),
            _ => env_or("CHATGPT_API_BASE", "https://chatgpt.com/backend-api/codex"),
        }
        .trim_end_matches('/')
        .to_string();
        let models: Vec<String> = env_or("OPENCC_MODELS", "")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Config {
            mode,
            port,
            fallback_model: env_or("OPENCC_FALLBACK_MODEL", ""),
            openai_api_key: env_or("OPENAI_API_KEY", ""),
            opencode_api_key: env_or("OPENCODE_API_KEY", ""),
            go_base_url,
            effort_policy_path: {
                let p = env_or("OPENCC_EFFORT_POLICY_FILE", "");
                if p.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(p))
                }
            },
            models,
            codex_auth_path: state::codex_auth_path(),
            api_base,
            auth_endpoint: env_or("OPENAI_AUTH_BASE", "https://auth.openai.com/oauth/token"),
        }
    }
}

// ── Usage conversion ───────────────────────────────────────────────────────────
// OpenAI includes the cached tokens in the input_tokens total and breaks them
// down in input_tokens_details.cached_tokens; Anthropic wants them separate
// (cache_read_input_tokens). Without this conversion /usage would show
// inflated input and zero cache for the openai backend.

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

pub fn extract_usage(usage: Option<&Value>) -> Usage {
    let usage = usage.unwrap_or(&Value::Null);
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let input_total = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Usage {
        input_tokens: input_total.saturating_sub(cached),
        output_tokens: usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    }
}

fn get_context_window_for_model(model: &str) -> Option<u64> {
    // ignore any @effort suffix
    let id = model.split('@').next().unwrap_or("");
    MODEL_CONTEXT_WINDOWS
        .iter()
        .find(|(m, _)| *m == id)
        .map(|(_, c)| *c)
}

fn build_usage_payload(model: &str, usage: &Usage) -> Value {
    let context_window = get_context_window_for_model(model);
    let total_input =
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;
    let mut payload = json!({
        "model": model,
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_input_tokens": total_input,
        "total_output_tokens": usage.output_tokens,
        "current_usage": {
            "input_tokens": usage.input_tokens,
            "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            "cache_read_input_tokens": usage.cache_read_input_tokens,
        },
    });
    if let Some(cw) = context_window {
        payload["context_window"] = json!(cw);
        payload["context_window_size"] = json!(cw);
        payload["used_percentage"] = json!((total_input as f64) / (cw as f64) * 100.0);
    }
    payload
}

// ── Auth: API key or Codex CLI OAuth token ─────────────────────────────────────

fn read_auth_file(path: &std::path::Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn resolve_auth(config: &Config) -> Option<(String, Option<String>)> {
    if config.mode == "apikey" {
        return if config.openai_api_key.is_empty() {
            None
        } else {
            Some((config.openai_api_key.clone(), None))
        };
    }
    let auth = read_auth_file(&config.codex_auth_path)?;
    let tokens = auth.get("tokens")?;
    let access = tokens.get("access_token")?.as_str()?;
    let account_id = tokens
        .get("account_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    Some((access.to_string(), account_id))
}

fn auth_hint(mode: &str) -> &'static str {
    if mode == "subscription" {
        "OAuth token expired. Run `opencc login` (or `codex login --device-auth`)."
    } else {
        "Set the OPENAI_API_KEY environment variable."
    }
}

/// Renews the access_token via refresh_token and rewrites ~/.codex/auth.json.
/// Returns (new access_token, account_id), or None on error.
async fn refresh_auth(
    config: &Config,
    auth: &Value,
    client: &reqwest::Client,
) -> Option<(String, Option<String>)> {
    let refresh_token = auth.pointer("/tokens/refresh_token")?.as_str()?;
    let client_id = auth
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .and_then(jwt_payload)
        .and_then(|p| {
            p.get("client_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| CODEX_CLIENT_ID.to_string());
    let res = client
        .post(&config.auth_endpoint)
        .header("Content-Type", "application/json")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": client_id,
        }))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    let data: Value = res.json().await.ok()?;
    let new_access = data.get("access_token")?.as_str()?;

    let mut new_auth = auth.clone();
    new_auth["last_refresh"] = json!(crate::util::iso_now());
    let mut tokens = auth.get("tokens").cloned().unwrap_or(json!({}));
    tokens["access_token"] = json!(new_access);
    if let Some(rt) = data.get("refresh_token").and_then(|v| v.as_str()) {
        tokens["refresh_token"] = json!(rt);
    }
    if let Some(idt) = data.get("id_token").and_then(|v| v.as_str()) {
        tokens["id_token"] = json!(idt);
    }
    if let Some(aid) = data.get("account_id").and_then(|v| v.as_str()) {
        tokens["account_id"] = json!(aid);
    }
    new_auth["tokens"] = tokens;
    let account_id = new_auth["tokens"]["account_id"].as_str().map(String::from);
    if let Ok(text) = serde_json::to_string_pretty(&new_auth) {
        let _ = state::atomic_write(&config.codex_auth_path, &format!("{text}\n"));
    }
    Some((new_access.to_string(), account_id))
}

// ── Request translation ────────────────────────────────────────────────────────

fn effort_decision_for(
    config: &Config,
    spec: &ModelSpec,
    requested: Option<&str>,
) -> EffortDecision {
    let policy = config
        .effort_policy_path
        .as_deref()
        .and_then(|p| read_effort_policy(p, &spec.id));
    normalize_effort(requested, policy.as_ref())
}

/// Applies the model's policy: rewrites `body.model` and
/// `output_config.effort` in place, logging the change when the effort was
/// adjusted.
fn normalize_messages_body(config: &Config, body: &mut Value, spec: &ModelSpec) -> EffortDecision {
    let requested = spec.effort.as_deref().or_else(|| {
        body.get("output_config")
            .and_then(|o| o.get("effort"))
            .and_then(|v| v.as_str())
    });
    let decision = effort_decision_for(config, spec, requested);
    if decision.requested != decision.applied {
        eprintln!(
            "[opencc] effort {}: {} -> {} ({})",
            spec.id,
            decision.requested.as_deref().unwrap_or("(none)"),
            decision.applied.as_deref().unwrap_or("(removed)"),
            decision.reason
        );
    }

    body["model"] = json!(spec.id);
    if let Some(output_config) = body.get_mut("output_config") {
        if let Some(obj) = output_config.as_object_mut() {
            obj.remove("effort");
            if obj.is_empty() {
                body.as_object_mut().map(|o| o.remove("output_config"));
            }
        }
    }
    if let Some(applied) = &decision.applied {
        // Merge into the remaining output_config (e.g. keeping `format`),
        // like the node proxy's spread.
        match body
            .get_mut("output_config")
            .and_then(|o| o.as_object_mut())
        {
            Some(obj) => {
                obj.insert("effort".to_string(), json!(applied));
            }
            None => {
                body["output_config"] = json!({ "effort": applied });
            }
        }
    }
    decision
}

/// Builds the Responses API `input` items from the Anthropic messages. Used
/// both for the request and for the turn-chaining extension check: it must
/// therefore produce canonical, deterministic items.
pub fn build_input_items(messages: Option<&Value>) -> Vec<Value> {
    let mut input = Vec::new();
    for msg in messages.and_then(|m| m.as_array()).unwrap_or(&Vec::new()) {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        match msg.get("content") {
            Some(Value::String(text)) => {
                input.push(json!({ "role": role, "content": text }));
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            input.push(json!({ "role": role, "content": text }));
                        }
                        Some("tool_use") => {
                            let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let args = block.get("input").cloned().unwrap_or(json!({}));
                            input.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": args.to_string(),
                            }));
                        }
                        Some("tool_result") => {
                            let call_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let content = match block.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Array(blocks)) => blocks
                                    .iter()
                                    .filter(|b| {
                                        b.get("type").and_then(|v| v.as_str()) == Some("text")
                                    })
                                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                                    .collect::<Vec<_>>()
                                    .join(""),
                                _ => String::new(),
                            };
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": call_id,
                                "output": content,
                            }));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    input
}

fn system_text(body: &Value) -> String {
    match body.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn build_responses_api_request(body: &Value, model_effort: Option<&str>) -> Value {
    let input = build_input_items(body.get("messages"));

    let mut req = json!({
        "model": body.get("model").and_then(|v| v.as_str()).unwrap_or(""),
        "input": input,
        // The ChatGPT backend requires store=false and stream=true; the
        // OpenAI API accepts the same parameters. Non-stream clients are
        // handled by collecting the SSE events.
        "store": false,
        "stream": true,
    });

    let instructions = system_text(body);
    req["instructions"] = json!(if instructions.is_empty() {
        "You are a helpful assistant.".to_string()
    } else {
        instructions
    });

    // The effort arrives as output_config.effort (already normalized and
    // applied by normalize_messages_body).
    if let Some(effort) = model_effort {
        if !effort.is_empty() {
            req["reasoning"] = json!({ "effort": effort });
        }
    }

    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        if !tools.is_empty() {
            let mapped: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        "description": t.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                        "parameters": t.get("input_schema").cloned()
                            .unwrap_or(json!({"type": "object", "properties": {}})),
                    })
                })
                .collect();
            req["tools"] = json!(mapped);
        }
    }

    // The OpenAI backends reject token-limit parameters: do NOT include
    // max_output_tokens.
    req
}

/// JSON round-trip of the tool arguments: deterministic, so it matches what
/// Claude Code resends.
pub fn normalize_arguments(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => v.to_string(),
        Err(_) => "{}".to_string(),
    }
}

/// Request properties that must be identical for a conversation to chain:
/// model, instructions and tools.
fn canonical_props(body: &Value, spec: &ModelSpec) -> String {
    let tools: Vec<Value> = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        "description": t.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                        "parameters": t.get("input_schema").cloned()
                            .unwrap_or(json!({"type": "object", "properties": {}})),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "model": spec.id,
        "instructions": system_text(body),
        "tools": tools,
    })
    .to_string()
}

/// True if `input` starts with `baseline` (item-for-item).
pub fn is_extension(baseline: &[Value], input: &[Value]) -> bool {
    if input.len() < baseline.len() {
        return false;
    }
    baseline.iter().zip(input.iter()).all(|(b, i)| b == i)
}

// ── Turn chaining ──────────────────────────────────────────────────────────────
// Codex never resends the full history: it checks that the new request is an
// extension of the previous one and sends only the delta with
// previous_response_id; the server reconnects the context and bills the
// repeated part at cache rates. State is keyed by session+agent
// (x-claude-code-session-id and x-claude-code-agent-id).

#[derive(Clone)]
struct ConvState {
    last_response_id: String,
    last_input: Vec<Value>,
    last_response_items: Vec<Value>,
    props: String,
}

#[derive(Default)]
struct ConversationStore {
    map: Mutex<HashMap<String, ConvState>>,
}

impl ConversationStore {
    fn get(&self, key: &str) -> Option<ConvState> {
        self.map.lock().unwrap().get(key).cloned()
    }
    fn remember(&self, key: &str, state: ConvState) {
        self.map.lock().unwrap().insert(key.to_string(), state);
    }
    fn forget(&self, key: &str) {
        self.map.lock().unwrap().remove(key);
    }
}

fn session_key(headers: &hyper::header::HeaderMap) -> Option<String> {
    let session = headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if session.is_empty() {
        return None;
    }
    let agent = headers
        .get("x-claude-code-agent-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    Some(format!("{session}|{agent}"))
}

// ── SSE helpers ────────────────────────────────────────────────────────────────

/// Accumulates raw chunks, splitting complete lines on `\n` (chunks can split
/// UTF-8 code points and JSON lines).
struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feeds a chunk; returns the complete lines (without the trailing `\n`).
    fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0;
        for (i, &b) in self.buffer.iter().enumerate() {
            if b == b'\n' {
                lines.push(self.buffer[start..i].to_vec());
                start = i + 1;
            }
        }
        self.buffer = self.buffer[start..].to_vec();
        lines
    }
}

fn format_sse(event: &str, data: &Value) -> Vec<u8> {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    format!("event: {event}\ndata: {json}\n\n").into_bytes()
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

fn u(v: &Value, k: &str) -> Option<usize> {
    v.get(k).and_then(|x| x.as_u64()).map(|n| n as usize)
}

// ── HTTP server ────────────────────────────────────────────────────────────────

type Shared = Arc<ServerCtx>;

struct ServerCtx {
    config: Config,
    conversations: ConversationStore,
    client: reqwest::Client,
}

pub async fn run(config: Config) -> Result<(), std::io::Error> {
    let ctx = Arc::new(ServerCtx {
        conversations: ConversationStore::default(),
        client: reqwest::Client::new(),
        config,
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", ctx.config.port)).await?;
    eprintln!(
        "[opencc-proxy] listening on http://127.0.0.1:{} (mode={})",
        ctx.config.port, ctx.config.mode
    );
    eprintln!("[opencc-proxy] upstream={}", ctx.config.api_base);
    loop {
        let (stream, _) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| handle(req, ctx.clone()));
            if let Err(err) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await
            {
                eprintln!("[opencc-proxy] connection error: {err}");
            }
        });
    }
}

async fn handle(
    req: Request<hyper::body::Incoming>,
    ctx: Shared,
) -> Result<Response<ResBody>, Infallible> {
    let config = &ctx.config;
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET && (path == "/health" || path == "/healthz") {
        let body = json!({
            "ok": true,
            "mode": config.mode,
            "port": config.port,
            "version": PROXY_VERSION,
            "fallback": config.fallback_model,
        });
        return Ok(json_response(StatusCode::OK, &body).await);
    }

    if method == Method::GET && (path == "/v1/models" || path == "/models") {
        let data: Vec<Value> = config
            .models
            .iter()
            .map(|id| json!({ "id": id, "object": "model", "owned_by": "openai" }))
            .collect();
        return Ok(json_response(StatusCode::OK, &json!({ "object": "list", "data": data })).await);
    }

    if method != Method::POST || !path.contains("/messages") {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            &json!({"error": {"message": "Not Found"}}),
        )
        .await);
    }

    let client_headers = req.headers().clone();
    let body_bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": "Bad Request"}}),
            )
            .await)
        }
    };
    let body: Value = match serde_json::from_slice(&body_bytes) {
        Ok(b) => b,
        Err(_) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"type": "invalid_request_error", "message": "Invalid JSON"}}),
            )
            .await)
        }
    };

    if config.mode == "opencode" {
        Ok(pipe_opencode_pass_through(ctx, client_headers, &path, &body).await)
    } else {
        Ok(handle_openai(ctx, client_headers, &body).await)
    }
}

async fn json_response(status: StatusCode, body: &Value) -> Response<ResBody> {
    // Capacity 1: send_data must complete before the response is handed to
    // hyper (a 0-capacity channel would deadlock until the receiver is polled).
    let (mut sender, body_channel) = Channel::new(1);
    let bytes = Bytes::from(serde_json::to_string(body).unwrap_or_else(|_| "{}".into()));
    let _ = sender.send_data(bytes).await;
    let mut resp = Response::new(body_channel);
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        "Content-Type",
        hyper::header::HeaderValue::from_static("application/json"),
    );
    resp
}

/// Extracts the error message from an upstream error body (JSON or raw text).
fn upstream_error_message(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(m) = v.pointer("/error/message").and_then(|x| x.as_str()) {
            return m.to_string();
        }
    }
    text.to_string()
}

/// Resolves a model spec: `claude-*` requests → the chosen fallback model
/// (which may itself carry `@effort`).
fn resolve_model_spec(config: &Config, model: &str) -> Option<ModelSpec> {
    let spec = parse_model_spec(model);
    if is_claude_model(&spec.id) {
        if config.fallback_model.is_empty() {
            return None;
        }
        Some(parse_model_spec(&config.fallback_model))
    } else {
        Some(spec)
    }
}

// ── opencode pass-through ──────────────────────────────────────────────────────
// The proxy only modifies `model` and `output_config.effort` (applying the
// policy) and forwards the rest of the request and response untranslated.

fn copy_forwarded_headers(upstream_headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    let skip = [
        "connection",
        "content-length",
        "transfer-encoding",
        "content-encoding",
    ];
    upstream_headers
        .iter()
        .filter(|(name, _)| !skip.contains(&name.as_str().to_ascii_lowercase().as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect()
}

async fn pipe_opencode_pass_through(
    ctx: Shared,
    client_headers: hyper::header::HeaderMap,
    path: &str,
    body: &Value,
) -> Response<ResBody> {
    if ctx.config.opencode_api_key.is_empty() {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({"error": {"type": "authentication_error", "message": "OPENCODE_API_KEY is missing."}}),
        )
        .await;
    }
    let requested_model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let Some(spec) = resolve_model_spec(&ctx.config, requested_model) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": {"type": "invalid_request_error", "message": format!("Model '{requested_model}' not handled.")}}),
        )
        .await;
    };
    let mut normalized = body.clone();
    normalize_messages_body(&ctx.config, &mut normalized, &spec);

    let mut upstream_builder = ctx
        .client
        .post(format!("{}{}", ctx.config.go_base_url, path))
        .header("Content-Type", "application/json")
        .header("x-api-key", &ctx.config.opencode_api_key)
        // No upstream compression: prevents the pass-through from forwarding
        // a Content-Encoding on a body already decompressed by the client.
        .header("Accept-Encoding", "identity")
        .json(&normalized);
    for name in ["anthropic-version", "anthropic-beta"] {
        if let Some(v) = client_headers.get(name) {
            if let Ok(v) = v.to_str() {
                upstream_builder = upstream_builder.header(name, v);
            }
        }
    }

    let upstream = match upstream_builder.send().await {
        Ok(r) => r,
        Err(err) => {
            return json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": format!("OpenCode Go upstream error: {err}")}}),
            )
            .await;
        }
    };

    let status = upstream.status();
    let forwarded = copy_forwarded_headers(upstream.headers());
    let (mut sender, body) = Channel::new(16);
    let mut stream = upstream.bytes_stream();
    tokio::spawn(async move {
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { break };
            if sender.send_data(chunk).await.is_err() {
                break; // client gone
            }
        }
    });
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    for (name, value) in forwarded {
        if let Ok(v) = hyper::header::HeaderValue::from_str(&value) {
            resp.headers_mut().insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes())
                    .unwrap_or(hyper::header::CONTENT_TYPE),
                v,
            );
        }
    }
    resp
}

// ── OpenAI path: Anthropic request → Responses request, response → Anthropic ──

async fn handle_openai(
    ctx: Shared,
    headers: hyper::header::HeaderMap,
    body: &Value,
) -> Response<ResBody> {
    let requested_model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let Some(spec) = resolve_model_spec(&ctx.config, requested_model) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({
                "error": {
                    "type": "invalid_request_error",
                    "message": format!(
                        "Model '{requested_model}' not handled: set OPENCC_FALLBACK_MODEL to an OpenAI model."
                    ),
                }
            }),
        )
        .await;
    };

    let Some((token, account_id)) = resolve_auth(&ctx.config) else {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({
                "error": {
                    "type": "authentication_error",
                    "message": format!("Authentication not found. {}", auth_hint(&ctx.config.mode)),
                }
            }),
        )
        .await;
    };

    let mut normalized = body.clone();
    let decision = normalize_messages_body(&ctx.config, &mut normalized, &spec);
    let mut responses_req = build_responses_api_request(&normalized, decision.applied.as_deref());
    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Turn chaining: if the request is an extension of the previous one for
    // the same session, we send only the delta with previous_response_id
    // (like codex does). On upstream error we fall back to the full request
    // without chaining.
    let input_items: Vec<Value> = responses_req
        .get("input")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    let props = canonical_props(&normalized, &spec);
    let key = if is_stream {
        session_key(&headers)
    } else {
        None
    };
    let mut linked = false;
    let mut delta_len = 0usize;
    let mut baseline_len = 0usize;
    if let Some(key) = &key {
        let mut props_changed = false;
        if let Some(conv) = ctx.conversations.get(key) {
            if !conv.last_response_id.is_empty() && conv.props == props {
                let mut baseline = conv.last_input.clone();
                baseline.extend(conv.last_response_items.clone());
                baseline_len = baseline.len();
                if is_extension(&baseline, &input_items) {
                    let delta = &input_items[baseline_len..];
                    if !delta.is_empty() {
                        linked = true;
                        delta_len = delta.len();
                        responses_req["previous_response_id"] = json!(conv.last_response_id);
                        responses_req["input"] = json!(delta);
                    }
                }
            } else {
                props_changed = true; // context changed (model, system or tools)
            }
        }
        if !linked && props_changed {
            ctx.conversations.forget(key);
        }
    }
    if linked {
        eprintln!(
            "[opencc] delta {}: {} items sent (baseline {})",
            key.as_deref().unwrap_or(""),
            delta_len,
            baseline_len
        );
    }

    let do_fetch = |token: &str, account_id: &Option<String>| {
        let mut builder = ctx
            .client
            .post(format!("{}/responses", ctx.config.api_base))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {token}"))
            .json(&responses_req);
        if let Some(aid) = account_id {
            builder = builder.header("ChatGPT-Account-ID", aid);
        }
        builder
    };

    let full_fetch = |token: &str, account_id: &Option<String>| {
        // Retry without chaining: full input and no previous_response_id.
        let mut full_req = responses_req.clone();
        full_req
            .as_object_mut()
            .map(|o| o.remove("previous_response_id"));
        full_req["input"] = json!(input_items);
        let mut builder = ctx
            .client
            .post(format!("{}/responses", ctx.config.api_base))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {token}"))
            .json(&full_req);
        if let Some(aid) = account_id {
            builder = builder.header("ChatGPT-Account-ID", aid);
        }
        builder
    };

    let mut upstream = match do_fetch(&token, &account_id).send().await {
        Ok(r) => r,
        Err(err) => {
            return json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": format!("Upstream error: {err}")}}),
            )
            .await;
        }
    };
    let mut auth = (token, account_id);
    // Expired OAuth token: try renewing it with the refresh_token and retry.
    if upstream.status() == StatusCode::UNAUTHORIZED && ctx.config.mode == "subscription" {
        if let Some(auth_file) = read_auth_file(&ctx.config.codex_auth_path) {
            if let Some((fresh, fresh_account)) =
                refresh_auth(&ctx.config, &auth_file, &ctx.client).await
            {
                auth = (fresh, fresh_account);
                upstream = match do_fetch(&auth.0, &auth.1).send().await {
                    Ok(r) => r,
                    Err(err) => {
                        return json_response(
                            StatusCode::BAD_GATEWAY,
                            &json!({"error": {"message": format!("Upstream error: {err}")}}),
                        )
                        .await;
                    }
                };
            }
        }
    }
    // Chaining can fail (e.g. response expired server-side): retry with the
    // full request and reset the conversation state.
    if linked && !upstream.status().is_success() {
        eprintln!(
            "[opencc] delta failed ({}): retrying without chaining",
            upstream.status()
        );
        upstream = match full_fetch(&auth.0, &auth.1).send().await {
            Ok(r) => r,
            Err(err) => {
                return json_response(
                    StatusCode::BAD_GATEWAY,
                    &json!({"error": {"message": format!("Upstream error: {err}")}}),
                )
                .await;
            }
        };
        if let Some(key) = &key {
            ctx.conversations.forget(key);
        }
        linked = false;
    }

    let status = upstream.status();
    if !status.is_success() {
        let err_text = upstream.text().await.unwrap_or_default();
        let mut message = upstream_error_message(&err_text);
        if status == StatusCode::UNAUTHORIZED {
            message = format!("{message} {}", auth_hint(&ctx.config.mode));
        }
        return json_response(status, &json!({"error": {"message": message}})).await;
    }

    // The model shown to the client is the one it sent (the node proxy's
    // `originalModel`), so /usage and the message id look right to it.
    let original_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if is_stream {
        let (sender, body) = Channel::new(64);
        let mut resp = Response::new(body);
        *resp.status_mut() = StatusCode::OK;
        resp.headers_mut().insert(
            "Content-Type",
            hyper::header::HeaderValue::from_static("text/event-stream"),
        );
        resp.headers_mut().insert(
            "Cache-Control",
            hyper::header::HeaderValue::from_static("no-cache"),
        );
        resp.headers_mut().insert(
            "Connection",
            hyper::header::HeaderValue::from_static("keep-alive"),
        );
        tokio::spawn(async move {
            stream_translation(
                ctx.clone(),
                upstream,
                sender,
                &original_model,
                &spec,
                ChainContext {
                    key,
                    linked,
                    input_items,
                    props,
                },
            )
            .await;
        });
        resp
    } else {
        collect_response(upstream, &original_model).await
    }
}

// ── SSE streaming: Responses events → Anthropic events ─────────────────────────

struct StreamState {
    started: bool,
    msg_id: String,
    next_block_idx: u32,
    /// output_index → { block index, open }
    blocks: HashMap<usize, BlockInfo>,
    has_tool_use: bool,
    resp_texts: HashMap<usize, String>,
    resp_tool_calls: HashMap<usize, ToolCallInfo>,
    resp_id: Option<String>,
}

struct BlockInfo {
    idx: u32,
    open: bool,
}

struct ToolCallInfo {
    call_id: String,
    name: String,
    args: String,
}

impl StreamState {
    fn new() -> Self {
        StreamState {
            started: false,
            msg_id: format!("msg_{}", crate::util::now_ms()),
            next_block_idx: 0,
            blocks: HashMap::new(),
            has_tool_use: false,
            resp_texts: HashMap::new(),
            resp_tool_calls: HashMap::new(),
            resp_id: None,
        }
    }

    fn open_block(&mut self, output_index: usize, _kind: &str) -> u32 {
        self.next_block_idx += 1;
        let idx = self.next_block_idx - 1;
        self.blocks
            .insert(output_index, BlockInfo { idx, open: true });
        idx
    }

    fn close_block(&mut self, output_index: Option<usize>) {
        if let Some(oi) = output_index {
            if let Some(b) = self.blocks.get_mut(&oi) {
                b.open = false;
            }
        }
    }
}

/// Writes `message_start` + `ping` once, before the first content block.
async fn ensure_message_start(
    state: &mut StreamState,
    original_model: &str,
    sender: &mut Option<http_body_util::channel::Sender<Bytes>>,
) {
    if state.started {
        return;
    }
    state.started = true;
    let empty_usage = Usage::default();
    let Some(s) = sender.as_mut() else {
        return;
    };
    let msg = json!({
        "type": "message_start",
        "message": {
            "id": state.msg_id.clone(),
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": original_model,
            "stop_reason": Value::Null,
            "stop_sequence": Value::Null,
            "usage": build_usage_payload(original_model, &empty_usage),
        },
    });
    if s.send_data(Bytes::from(format_sse("message_start", &msg)))
        .await
        .is_err()
    {
        *sender = None;
        return;
    }
    if s.send_data(Bytes::from(format_sse("ping", &json!({"type": "ping"}))))
        .await
        .is_err()
    {
        *sender = None;
    }
}

/// State needed to remember (or forget) a conversation for turn chaining.
struct ChainContext {
    key: Option<String>,
    linked: bool,
    input_items: Vec<Value>,
    props: String,
}

/// Writes the upstream Responses SSE stream to the client as Anthropic SSE.
/// On client abort the upstream body is dropped.
async fn stream_translation(
    ctx: Shared,
    upstream: reqwest::Response,
    sender: http_body_util::channel::Sender<Bytes>,
    original_model: &str,
    spec: &ModelSpec,
    chain: ChainContext,
) {
    let mut state = StreamState::new();
    let mut parser = SseParser::new();
    let mut stream_usage: Option<Usage> = None;
    let mut sender: Option<http_body_util::channel::Sender<Bytes>> = Some(sender);

    macro_rules! emit {
        ($event:expr, $data:expr) => {
            if let Some(s) = sender.as_mut() {
                let bytes = Bytes::from(format_sse($event, &$data));
                if s.send_data(bytes).await.is_err() {
                    sender = None; // client gone: drop the upstream body
                }
            }
        };
    }

    let mut chunks = upstream.bytes_stream();
    loop {
        let Some(Ok(chunk)) = chunks.next().await else {
            break;
        };
        for line in parser.feed(&chunk) {
            if sender.is_none() {
                break;
            }
            if !line.starts_with(b"data: ") {
                continue; // comments (": keep-alive") and blank lines
            }
            let payload = std::str::from_utf8(&line[6..]).unwrap_or("").trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let evt: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match s(&evt, "type") {
                Some("response.completed") | Some("response.done") => {
                    let resp = evt.get("response").unwrap_or(&evt);
                    // /usage accumulates input/cache from the final
                    // message_delta usage: the full usage is needed, not just
                    // output_tokens.
                    stream_usage = Some(extract_usage(resp.get("usage")));
                    state.resp_id = resp.get("id").and_then(|v| v.as_str()).map(String::from);
                }
                Some("response.output_item.added") => {
                    if let Some(item) = evt.get("item") {
                        let oi = u(&evt, "output_index")
                            .or_else(|| u(item, "index"))
                            .unwrap_or(state.blocks.len());
                        if s(item, "type") == Some("function_call") {
                            ensure_message_start(&mut state, original_model, &mut sender).await;
                            let idx = state.open_block(oi, "tool_use");
                            state.has_tool_use = true;
                            let call_id = s(item, "call_id").unwrap_or("").to_string();
                            let id = s(item, "id").unwrap_or("").to_string();
                            let name = s(item, "name").unwrap_or("").to_string();
                            emit!(
                                "content_block_start",
                                json!({
                                    "type": "content_block_start",
                                    "index": idx,
                                    "content_block": {
                                        "type": "tool_use",
                                        "id": if call_id.is_empty() { id.clone() } else { call_id.clone() },
                                        "name": name,
                                        "input": {},
                                    },
                                })
                            );
                            state.resp_tool_calls.insert(
                                oi,
                                ToolCallInfo {
                                    call_id: if call_id.is_empty() { id } else { call_id },
                                    name,
                                    args: String::new(),
                                },
                            );
                        }
                    }
                }
                Some("response.output_item.done") => {
                    let oi = u(&evt, "output_index")
                        .or_else(|| evt.get("item").and_then(|i| u(i, "index")));
                    if let (Some(oi), Some(idx)) =
                        (oi, oi.and_then(|oi| state.blocks.get(&oi)).map(|b| b.idx))
                    {
                        state.close_block(Some(oi));
                        emit!(
                            "content_block_stop",
                            json!({
                                "type": "content_block_stop",
                                "index": idx,
                            })
                        );
                    }
                }
                Some("response.output_text.delta") => {
                    if let Some(delta) = s(&evt, "delta") {
                        let oi = u(&evt, "output_index").unwrap_or(0);
                        state
                            .resp_texts
                            .entry(oi)
                            .and_modify(|t| t.push_str(delta))
                            .or_insert_with(|| delta.to_string());
                        if !state.blocks.contains_key(&oi) {
                            ensure_message_start(&mut state, original_model, &mut sender).await;
                            let idx = state.open_block(oi, "text");
                            emit!(
                                "content_block_start",
                                json!({
                                    "type": "content_block_start",
                                    "index": idx,
                                    "content_block": { "type": "text", "text": "" },
                                })
                            );
                        }
                        let idx = state.blocks.get(&oi).map(|b| b.idx).unwrap_or(0);
                        emit!(
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta",
                                "index": idx,
                                "delta": { "type": "text_delta", "text": delta },
                            })
                        );
                    }
                }
                Some("response.function_call_arguments.delta") => {
                    if let Some(delta) = s(&evt, "delta") {
                        let oi = u(&evt, "output_index").unwrap_or(0);
                        if let Some(tool) = state.resp_tool_calls.get_mut(&oi) {
                            tool.args.push_str(delta);
                        }
                        let Some(idx) = state.blocks.get(&oi).map(|b| b.idx) else {
                            continue;
                        };
                        emit!(
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta",
                                "index": idx,
                                "delta": { "type": "input_json_delta", "partial_json": delta },
                            })
                        );
                    }
                }
                Some("response.function_call_arguments.done") => {
                    let oi = u(&evt, "output_index");
                    if let (Some(oi), Some(idx)) =
                        (oi, oi.and_then(|oi| state.blocks.get(&oi)).map(|b| b.idx))
                    {
                        state.close_block(Some(oi));
                        emit!(
                            "content_block_stop",
                            json!({
                                "type": "content_block_stop",
                                "index": idx,
                            })
                        );
                    }
                }
                _ => {}
            }
        }
        if sender.is_none() {
            break;
        }
    }

    // Close any block left open.
    let open_idxs: Vec<(usize, u32)> = state
        .blocks
        .iter()
        .filter(|(_, b)| b.open)
        .map(|(oi, b)| (*oi, b.idx))
        .collect();
    for (oi, idx) in open_idxs {
        state.close_block(Some(oi));
        emit!(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": idx,
            })
        );
    }

    // Remember the conversation for the next turn's chaining, and record the
    // usage for diagnostics (verifying the cache savings).
    let mut response_items: Vec<Value> = Vec::new();
    let mut all_idx: Vec<usize> = state
        .resp_texts
        .keys()
        .chain(state.resp_tool_calls.keys())
        .copied()
        .collect();
    all_idx.sort_unstable();
    for oi in all_idx {
        if let Some(text) = state.resp_texts.get(&oi) {
            response_items.push(json!({ "role": "assistant", "content": text }));
        } else if let Some(tool) = state.resp_tool_calls.get(&oi) {
            response_items.push(json!({
                "type": "function_call",
                "call_id": tool.call_id,
                "name": tool.name,
                "arguments": normalize_arguments(&tool.args),
            }));
        }
    }
    if let Some(key) = &chain.key {
        if let Some(resp_id) = state.resp_id.clone() {
            ctx.conversations.remember(
                key,
                ConvState {
                    last_response_id: resp_id,
                    last_input: chain.input_items,
                    last_response_items: response_items,
                    props: chain.props,
                },
            );
        }
    }
    let su = stream_usage.unwrap_or_default();
    eprintln!(
        "[opencc] usage {}: in={} cached={} out={} ({})",
        spec.id,
        su.input_tokens,
        su.cache_read_input_tokens,
        su.output_tokens,
        if chain.linked { "delta" } else { "full" }
    );

    if state.started {
        emit!(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": if state.has_tool_use { "tool_use" } else { "end_turn" },
                    "stop_sequence": Value::Null,
                },
                "usage": build_usage_payload(original_model, &su),
            })
        );
        emit!("message_stop", json!({ "type": "message_stop" }));
    } else {
        let empty_usage = Usage {
            output_tokens: 0,
            ..Default::default()
        };
        ensure_message_start(&mut state, original_model, &mut sender).await;
        emit!(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            })
        );
        emit!(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": 0,
            })
        );
        emit!(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": Value::Null },
                "usage": build_usage_payload(original_model, &empty_usage),
            })
        );
        emit!("message_stop", json!({ "type": "message_stop" }));
    }
    drop(sender); // end of the stream body
}

/// Non-stream client: collect the upstream SSE events and build a single
/// Anthropic JSON response.
async fn collect_response(upstream: reqwest::Response, original_model: &str) -> Response<ResBody> {
    let mut parser = SseParser::new();
    let mut collected_text = String::new();
    let mut collected_tool_calls: Vec<Value> = Vec::new();
    let mut collected_usage = Usage::default();

    let mut chunks = upstream.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let Ok(chunk) = chunk else { break };
        for line in parser.feed(&chunk) {
            if !line.starts_with(b"data: ") {
                continue;
            }
            let payload = std::str::from_utf8(&line[6..]).unwrap_or("").trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let evt: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match s(&evt, "type") {
                Some("response.output_text.delta") => {
                    if let Some(delta) = s(&evt, "delta") {
                        collected_text.push_str(delta);
                    }
                }
                Some("response.function_call_arguments.done") => {
                    let input =
                        match serde_json::from_str::<Value>(s(&evt, "arguments").unwrap_or("{}")) {
                            Ok(v) => v,
                            Err(_) => json!({}),
                        };
                    let call_id = s(&evt, "call_id").unwrap_or("").to_string();
                    collected_tool_calls.push(json!({
                        "type": "tool_use",
                        "id": if call_id.is_empty() {
                            format!("toolu_{}", crate::util::now_ms())
                        } else {
                            call_id
                        },
                        "name": s(&evt, "name").unwrap_or(""),
                        "input": input,
                    }));
                }
                Some("response.completed") | Some("response.done") => {
                    let r = evt.get("response").unwrap_or(&evt);
                    collected_usage = extract_usage(r.get("usage"));
                }
                _ => {}
            }
        }
    }

    let mut content: Vec<Value> = Vec::new();
    if !collected_text.is_empty() {
        content.push(json!({ "type": "text", "text": collected_text }));
    }
    for tc in &collected_tool_calls {
        content.push(tc.clone());
    }
    let has_tool_use = !collected_tool_calls.is_empty();

    if !content.is_empty() || collected_usage.output_tokens > 0 {
        json_response(
            StatusCode::OK,
            &json!({
                "id": format!("msg_{}", crate::util::now_ms()),
                "type": "message",
                "role": "assistant",
                "content": if content.is_empty() {
                    vec![json!({ "type": "text", "text": "" })]
                } else {
                    content
                },
                "model": original_model,
                "stop_reason": if has_tool_use { "tool_use" } else { "end_turn" },
                "stop_sequence": Value::Null,
                "usage": build_usage_payload(original_model, &collected_usage),
            }),
        )
        .await
    } else {
        json_response(
            StatusCode::BAD_GATEWAY,
            &json!({"error": {"message": "No response received from the OpenAI backend"}}),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_claude_aliases() {
        assert!(is_claude_model("claude-opus-4-1"));
        assert!(is_claude_model("opus-4-1"));
        assert!(is_claude_model("haiku"));
        assert!(!is_claude_model("gpt-5.6-sol"));
        assert!(!is_claude_model("minimax-m3"));
        assert!(!is_claude_model("openopus"));
    }

    #[test]
    fn builds_deterministic_input_items() {
        let messages = json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "out"}]}]},
        ]);
        let items = build_input_items(Some(&messages));
        assert_eq!(items[0], json!({"role": "user", "content": "hello"}));
        assert_eq!(items[1], json!({"role": "assistant", "content": "hi"}));
        assert_eq!(
            items[2],
            json!({"type": "function_call_output", "call_id": "t1", "output": "out"})
        );
        // Deterministic: same input → same output.
        assert_eq!(items, build_input_items(Some(&messages)));
        // Missing messages → empty input.
        assert!(build_input_items(None).is_empty());
    }

    #[test]
    fn recognizes_conversation_extensions() {
        let base = vec![json!({"role": "user", "content": "hello"})];
        let full = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "and then?"}),
        ];
        assert!(is_extension(&base, &full));
        assert!(is_extension(&base, &base));
        assert!(!is_extension(&full, &base));
        assert!(!is_extension(
            &[json!({"role": "user", "content": "different"})],
            &full
        ));
        assert_eq!(
            normalize_arguments("{\"a\":1,\"b\":[2]}"),
            "{\"a\":1,\"b\":[2]}"
        );
        assert_eq!(normalize_arguments("non-json"), "{}");
    }

    #[test]
    fn converts_usage_like_the_node_proxy() {
        let usage = extract_usage(Some(&json!({
            "input_tokens": 120,
            "output_tokens": 40,
            "input_tokens_details": {"cached_tokens": 20},
            "output_tokens_details": {"reasoning_tokens": 5},
        })));
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(usage.cache_read_input_tokens, 20);
        assert_eq!(usage.cache_creation_input_tokens, 0);

        let empty = extract_usage(None);
        assert_eq!(empty.input_tokens, 0);
        assert_eq!(empty.output_tokens, 0);
        assert_eq!(empty.cache_read_input_tokens, 0);

        // The payload carries the Anthropic totals and the context window.
        let payload = build_usage_payload("gpt-5.4", &usage);
        assert_eq!(payload["input_tokens"], 100);
        assert_eq!(payload["total_input_tokens"], 120);
        assert_eq!(payload["context_window"], 950000);
        assert_eq!(payload["current_usage"]["cache_read_input_tokens"], 20);
    }

    #[test]
    fn parses_sse_streams() {
        let mut parser = SseParser::new();
        // Chunks split lines and UTF-8 (the boundary falls inside the JSON).
        let mut lines = parser.feed(b"data: {\"a\":1}\n\ndata: {\"b");
        assert_eq!(lines.len(), 2);
        lines = parser.feed(b"\":2}\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(&lines[0], b"data: {\"b\":2}");
        // A trailing partial line is kept until its \n arrives.
        lines = parser.feed(b"data: x");
        assert!(lines.is_empty());
        lines = parser.feed(b"\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(&lines[0], b"data: x");
    }

    #[test]
    fn formats_sse_events() {
        let bytes = format_sse("ping", &json!({"type": "ping"}));
        assert_eq!(bytes, b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
    }

    #[test]
    fn normalizes_the_messages_body() {
        let mut config = Config::from_env();
        config.effort_policy_path = None;
        let mut body = json!({
            "model": "gpt-two",
            "messages": [{"role": "user", "content": "hi"}],
            "output_config": {"effort": "xhigh", "format": {"type": "json_schema"}},
        });
        let spec = parse_model_spec("gpt-two");
        let decision = normalize_messages_body(&config, &mut body, &spec);
        assert_eq!(body["model"], "gpt-two");
        // Without a policy the effort passes through untouched.
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(decision.applied.as_deref(), Some("xhigh"));
    }

    #[test]
    fn builds_the_responses_request() {
        let body = json!({
            "model": "gpt-one",
            "system": "be nice",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{"name": "f", "description": "d", "input_schema": {"type": "object"}}],
        });
        let req = build_responses_api_request(&body, Some("xhigh"));
        assert_eq!(req["model"], "gpt-one");
        assert_eq!(req["store"], false);
        assert_eq!(req["stream"], true);
        assert_eq!(req["instructions"], "be nice");
        assert_eq!(req["reasoning"], json!({"effort": "xhigh"}));
        assert_eq!(
            req["tools"][0],
            json!({
                "type": "function", "name": "f", "description": "d",
                "parameters": {"type": "object"},
            })
        );
        // Default instructions when none is provided.
        let req = build_responses_api_request(&json!({"model": "gpt-one", "messages": []}), None);
        assert_eq!(req["instructions"], "You are a helpful assistant.");
        assert!(req.get("reasoning").is_none());
    }
}
