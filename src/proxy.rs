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
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Channel};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMessage};

/// The upstream WebSocket connection type (TLS or plain, per scheme).
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsWriter = SplitSink<WsStream, WsMessage>;

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
            .or_else(|| {
                usage
                    .pointer("/input_tokens_details/cache_write_tokens")
                    .and_then(|v| v.as_u64())
            })
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

/// Rough token estimate of a request as sent (chars / 4, the usual heuristic).
/// Used for the /usage cache columns: the ChatGPT backend does not report the
/// reconnected context on chained turns, so the proxy reports its own
/// estimate of the baseline instead.
fn estimate_request_tokens(req: &Value) -> u64 {
    let chars = serde_json::to_string(req)
        .map(|s| s.chars().count() as u64)
        .unwrap_or(0);
    chars / 4
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
    /// Estimated tokens of everything sent so far in this conversation: the
    /// size of the context the backend reconnects (and bills at cache rates)
    /// on the next chained turn.
    cumulative: u64,
}

/// Conversations kept for chaining. Each entry retains the full input items
/// of the last turn (every tool result verbatim), so an unbounded store grows
/// with every agent spawned and never shrinks. Cap it: a conversation past
/// the cap is one nobody is chaining onto any more.
const MAX_CONVERSATIONS: usize = 64;

/// A conversation untouched for this long is dropped. Claude Code will have
/// moved on; keeping it only holds its input items in memory.
const CONVERSATION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
struct ConversationStore {
    map: Mutex<HashMap<String, (ConvState, Instant)>>,
}

impl ConversationStore {
    fn get(&self, key: &str) -> Option<ConvState> {
        let mut map = self.map.lock().unwrap();
        let entry = map.get_mut(key)?;
        entry.1 = Instant::now();
        Some(entry.0.clone())
    }
    fn remember(&self, key: &str, state: ConvState) {
        let mut map = self.map.lock().unwrap();
        map.insert(key.to_string(), (state, Instant::now()));
        Self::prune(&mut map);
    }
    fn forget(&self, key: &str) {
        self.map.lock().unwrap().remove(key);
    }

    /// Drops idle conversations, then the least-recently-used ones until the
    /// store is back under the cap.
    fn prune(map: &mut HashMap<String, (ConvState, Instant)>) {
        let now = Instant::now();
        map.retain(|_, (_, seen)| now.duration_since(*seen) < CONVERSATION_IDLE_TIMEOUT);
        while map.len() > MAX_CONVERSATIONS {
            let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (_, seen))| *seen)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            map.remove(&oldest);
        }
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

// ── WebSocket upstream (subscription mode) ─────────────────────────────────────
// The ChatGPT backend rejects `previous_response_id` on the plain HTTP
// `/responses` endpoint ("Unsupported parameter"), so chaining over HTTP can
// never succeed: every turn would fall back to a full resend of the whole
// context at full input rate. The same backend serves the Responses protocol
// over WebSocket, where `previous_response_id` + a delta input work (this is
// how the codex CLI itself runs): the server reconnects the previous context
// and bills the repeated part at cache rates. The proxy therefore keeps one
// WebSocket connection per conversation (session|agent) and chains turns on
// it, falling back to the HTTP path when the connection cannot be established
// or the chained request fails.

/// The WebSocket endpoint for the Responses API: same base, scheme swapped.
fn ws_url_for(api_base: &str) -> String {
    let base = api_base.trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/responses")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}/responses")
    } else {
        format!("{base}/responses")
    }
}

/// One WebSocket connection to the upstream, shared by all requests of one
/// conversation. The reader task pumps server frames into `rx`; a request
/// takes the receiver, consumes exactly its own response (up to
/// response.completed), then puts it back.
struct WsEntry {
    /// Writer half of the connection (send serialized per frame).
    writer: Arc<tokio::sync::Mutex<WsWriter>>,
    /// Events from the reader task; None while a request is streaming.
    rx: Option<mpsc::Receiver<Value>>,
    /// Dropping this sender stops the reader task (which also drops the
    /// socket once the writer is gone).
    _abort: tokio::sync::watch::Sender<bool>,
    /// Bumped on reconnect; a late task must not put its receiver back into
    /// a newer entry.
    gen: u64,
    /// Set while a request streams on this connection.
    in_flight: bool,
    /// When this session last carried a turn, for idle eviction.
    last_used: Instant,
}

/// Live upstream WebSocket sessions kept at once. Every agent holds one for
/// the length of its conversation, and the backend caps concurrent
/// connections per account: without a cap here, spawning agents eventually
/// gets new connections rejected, and every rejection costs a full resend to
/// discover. Evicting the coldest session instead is far cheaper — it only
/// forfeits chaining for an agent that has gone quiet.
const MAX_WS_SESSIONS: usize = 24;

/// A session with no turn for this long is dropped. The backend closes idle
/// connections on its own; discovering that lazily costs a wedged turn.
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Upper bound on how long one turn may hold a session before it is assumed
/// leaked. Well past any real turn: this only reclaims connections whose
/// streaming task died without releasing them.
const WS_STUCK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a turn waits for a busy session before giving up on chaining.
/// Sized for the handover between two turns of the same agent, not for a
/// whole turn: past this it is cheaper to resend than to keep waiting.
const WS_BUSY_WAIT: Duration = Duration::from_secs(2);
const WS_BUSY_POLL: Duration = Duration::from_millis(25);

/// All session bookkeeping under one lock. `connecting` reserves a key while
/// its socket is being dialled, so two turns starting at once on a cold key
/// open one connection rather than racing to open two — the loser of that
/// race used to fall back to a full resend.
#[derive(Default)]
struct WsState {
    map: HashMap<String, WsEntry>,
    connecting: std::collections::HashSet<String>,
    next_gen: u64,
}

#[derive(Default)]
struct WsSessions {
    state: Mutex<WsState>,
}

impl WsSessions {
    /// Claims the connection for `key` for the length of one turn, connecting
    /// a fresh session when none exists. The entry is marked in-flight before
    /// the lock is released, so the claim is atomic: a caller that gets a
    /// `WsAcquired` owns the connection until it calls `release` or `kill`.
    ///
    /// `None` means "do not use the WebSocket for this turn": another turn
    /// holds the connection, or it could not be established.
    async fn acquire(
        &self,
        key: &str,
        config: &Config,
        token: &str,
        account_id: &Option<String>,
    ) -> Option<WsAcquired> {
        // A turn already streaming on this key is almost always the previous
        // turn of the same agent, about to finish. Waiting briefly for it
        // keeps this turn chained; giving up at once would resend the whole
        // conversation at full input rate to say the same thing.
        let deadline = Instant::now() + WS_BUSY_WAIT;
        let gen = loop {
            {
                let mut st = self.state.lock().unwrap();
                Self::evict_idle(&mut st.map);
                if let Some(acquired) = Self::claim(&mut st.map, key) {
                    return Some(acquired);
                }
                // Busy means either a turn is streaming on the session or
                // another turn is dialling one for this key. Both end soon;
                // wait rather than open a second connection.
                let streaming = st.map.get(key).is_some_and(|e| e.in_flight);
                if !streaming && !st.connecting.contains(key) {
                    // Any entry still here is dead — not in flight, yet
                    // missing its receiver. Drop it and dial a replacement,
                    // reserving the key so no one else dials in parallel.
                    st.map.remove(key);
                    st.connecting.insert(key.to_string());
                    st.next_gen += 1;
                    break st.next_gen;
                }
            }
            if Instant::now() >= deadline {
                return None; // still busy: this turn goes unchained
            }
            tokio::time::sleep(WS_BUSY_POLL).await;
        };

        let dialled = ws_connect(config, token, account_id).await;

        let mut st = self.state.lock().unwrap();
        st.connecting.remove(key);
        let (writer, rx, abort_tx) = dialled?;
        Self::evict_idle(&mut st.map);
        Self::make_room(&mut st.map);
        st.map.insert(
            key.to_string(),
            WsEntry {
                writer: writer.clone(),
                rx: None,
                _abort: abort_tx,
                gen,
                in_flight: true,
                last_used: Instant::now(),
            },
        );
        Some(WsAcquired { writer, rx, gen })
    }

    /// Takes the receiver of an idle live entry and marks it in flight.
    fn claim(map: &mut HashMap<String, WsEntry>, key: &str) -> Option<WsAcquired> {
        let entry = map.get_mut(key)?;
        if entry.in_flight {
            return None;
        }
        let rx = entry.rx.take()?;
        entry.in_flight = true;
        entry.last_used = Instant::now();
        Some(WsAcquired {
            writer: entry.writer.clone(),
            rx,
            gen: entry.gen,
        })
    }

    /// Drops sessions that have carried no turn recently.
    fn evict_idle(map: &mut HashMap<String, WsEntry>) {
        let now = Instant::now();
        map.retain(|_, e| {
            let idle = now.duration_since(e.last_used);
            if e.in_flight {
                // A turn holding the connection for longer than any turn can
                // plausibly run has leaked it (its task died without
                // releasing). Reaping it costs that turn nothing — it owns
                // the socket directly — and stops one lost task from
                // blocking the key, and the session cap, forever.
                idle < WS_STUCK_TIMEOUT
            } else {
                idle < WS_IDLE_TIMEOUT
            }
        });
    }

    /// Makes room for one more session by dropping the coldest idle one.
    fn make_room(map: &mut HashMap<String, WsEntry>) {
        while map.len() >= MAX_WS_SESSIONS {
            let Some(coldest) = map
                .iter()
                .filter(|(_, e)| !e.in_flight)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break; // everything is streaming: let the map exceed the cap
            };
            map.remove(&coldest);
        }
    }

    /// Marks the streaming request finished: puts the receiver back and
    /// clears the in-flight flag, unless the entry was replaced meanwhile.
    /// Only call this for a connection still known to be healthy — a dead
    /// receiver put back here is handed to the next turn, which then wedges
    /// on it and pays a full resend to find out.
    fn release(&self, key: &str, gen: u64, rx: mpsc::Receiver<Value>) {
        let mut st = self.state.lock().unwrap();
        if let Some(entry) = st.map.get_mut(key) {
            if entry.gen == gen {
                entry.in_flight = false;
                entry.last_used = Instant::now();
                entry.rx = Some(rx);
            }
        }
    }

    /// Marks the session dead (client abort, fatal error): the connection is
    /// dropped and the next request reconnects with a full resend.
    fn kill(&self, key: &str, gen: u64) {
        let mut st = self.state.lock().unwrap();
        if let Some(entry) = st.map.get(key) {
            if entry.gen == gen {
                st.map.remove(key);
            }
        }
    }
}

struct WsAcquired {
    writer: Arc<tokio::sync::Mutex<WsWriter>>,
    rx: mpsc::Receiver<Value>,
    gen: u64,
}

/// Connects the Responses WebSocket and spawns the reader task. Returns the
/// writer and the event receiver, or None on failure.
async fn ws_connect(
    config: &Config,
    token: &str,
    account_id: &Option<String>,
) -> Option<(
    Arc<tokio::sync::Mutex<WsWriter>>,
    mpsc::Receiver<Value>,
    tokio::sync::watch::Sender<bool>,
)> {
    let mut request = ws_url_for(&config.api_base).into_client_request().ok()?;
    let headers = request.headers_mut();
    headers.insert(
        "Authorization",
        hyper::header::HeaderValue::from_str(&format!("Bearer {token}")).ok()?,
    );
    if let Some(aid) = account_id {
        headers.insert(
            "ChatGPT-Account-ID",
            hyper::header::HeaderValue::from_str(aid).ok()?,
        );
    }
    // Same product marker the codex CLI sends.
    headers.insert(
        "OAI-Product-Sku",
        hyper::header::HeaderValue::from_static("codex"),
    );

    let connected = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .ok()?;
    let (stream, _resp) = connected.ok()?;
    let (writer, reader) = stream.split();
    let (tx, rx) = mpsc::channel::<Value>(64);
    let (abort_tx, mut abort_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut reader = reader;
        loop {
            tokio::select! {
                _ = abort_rx.changed() => break,
                msg = reader.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            if let Ok(evt) = serde_json::from_str::<Value>(text.as_str()) {
                                if tx.send(evt).await.is_err() {
                                    break; // receiver gone: connection abandoned
                                }
                            }
                        }
                        // Close/ping/pong/binary frames need no translation.
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    }
                }
            }
        }
    });
    Some((Arc::new(tokio::sync::Mutex::new(writer)), rx, abort_tx))
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
    ws_sessions: WsSessions,
    client: reqwest::Client,
    /// Cleared for the process if the upstream ever rejects
    /// `prompt_cache_key`. The ChatGPT backend is stricter about unknown
    /// parameters than the public API, so the flag is discovered rather than
    /// assumed.
    send_cache_key: AtomicBool,
}

pub async fn run(config: Config) -> Result<(), std::io::Error> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        // Per-read, not per-request: a long stream is fine, a stalled one is
        // not. Without this a wedged upstream pins the turn forever.
        .read_timeout(Duration::from_secs(120))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let ctx = Arc::new(ServerCtx {
        conversations: ConversationStore::default(),
        ws_sessions: WsSessions::default(),
        client,
        send_cache_key: AtomicBool::new(true),
        config,
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", ctx.config.port)).await?;
    eprintln!(
        "[opencc-proxy] listening on http://127.0.0.1:{} (mode={})",
        ctx.config.port, ctx.config.mode
    );
    eprintln!("[opencc-proxy] upstream={}", ctx.config.api_base);
    loop {
        // A failed accept (fd exhaustion under many agents, a connection
        // reset during the handshake) is transient. Returning here would
        // take the whole proxy down and strand every running agent.
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("[opencc-proxy] accept error: {err}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
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

/// Outcome of one WebSocket request attempt.
enum WsAttempt {
    /// The response is streaming to the client.
    Streamed(Response<ResBody>),
    /// The upstream rejected the request (stale previous_response_id,
    /// connection limit): reconnect and retry with the full input.
    RetryFull,
    /// The WebSocket path is not usable right now: fall back to HTTP.
    Fallback,
}

/// Runs one request on the conversation's WebSocket connection and streams
/// the translation. Reconnects when the connection is gone.
async fn ws_attempt(
    ctx: &Shared,
    key: &str,
    responses_req: &Value,
    original_model: String,
    spec: ModelSpec,
    chain: ChainContext,
) -> WsAttempt {
    let Some((token, account_id)) = resolve_auth(&ctx.config) else {
        return WsAttempt::Fallback;
    };
    let Some(acquired) = ctx
        .ws_sessions
        .acquire(key, &ctx.config, &token, &account_id)
        .await
    else {
        return WsAttempt::Fallback;
    };
    // `acquire` already marked the entry in flight: this turn owns the
    // connection until it releases or kills it.
    let gen = acquired.gen;

    let mut frame = responses_req.clone();
    frame["type"] = json!("response.create");
    let frame_text = frame.to_string();
    let send_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        acquired
            .writer
            .lock()
            .await
            .send(WsMessage::Text(frame_text.into())),
    )
    .await;
    if !matches!(send_result, Ok(Ok(()))) {
        ctx.ws_sessions.kill(key, gen);
        return WsAttempt::Fallback;
    }

    // The first event decides: an error frame (invalid previous_response_id,
    // 60-minute connection limit) means the request was rejected → retry the
    // full input on a fresh connection; anything else starts the stream.
    let mut rx = acquired.rx;
    let first = match tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await {
        Ok(Some(evt)) => evt,
        _ => {
            ctx.ws_sessions.kill(key, gen);
            return WsAttempt::Fallback;
        }
    };
    if s(&first, "type") == Some("error") {
        // The request was rejected (stale previous_response_id, connection
        // limit): reconnect and retry with the full input.
        ctx.ws_sessions.kill(key, gen);
        return if chain.linked {
            WsAttempt::RetryFull
        } else {
            WsAttempt::Fallback
        };
    }
    // response.failed (quota, policy) streams normally: the translation
    // relays it to the client as an Anthropic error event, without burning
    // another request on a retry.

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
    let ctx2 = ctx.clone();
    let key_owned = key.to_string();
    tokio::spawn(async move {
        ws_stream_translation(
            ctx2,
            key_owned,
            gen,
            first,
            rx,
            sender,
            &original_model,
            &spec,
            chain,
        )
        .await;
    });
    WsAttempt::Streamed(resp)
}

/// WebSocket path for subscription mode: chains turns with
/// previous_response_id + delta on a persistent connection (the ChatGPT
/// backend only supports chaining over WebSocket). Returns None when the
/// path is unusable, restoring `responses_req` to the full input so the
/// caller can fall back to HTTP.
#[allow(clippy::too_many_arguments)] // internal plumbing: request context
async fn ws_handle(
    ctx: Shared,
    key: String,
    responses_req: &mut Value,
    input_items: &[Value],
    props: String,
    linked: bool,
    cached_est: u64,
    original_model: &str,
    spec: &ModelSpec,
    _headers: &hyper::header::HeaderMap,
) -> Option<Response<ResBody>> {
    // Attempt 1: chained (delta + previous_response_id) when possible.
    let chain = ChainContext {
        key: Some(key.clone()),
        linked,
        input_items: input_items.to_vec(),
        props: props.clone(),
        est_this: estimate_request_tokens(responses_req),
        cached_est,
    };
    match ws_attempt(
        &ctx,
        &key,
        responses_req,
        original_model.to_string(),
        spec.clone(),
        chain,
    )
    .await
    {
        WsAttempt::Streamed(resp) => return Some(resp),
        WsAttempt::RetryFull => {
            // The connection state is stale: reconnect with the full input.
            ctx.conversations.forget(&key);
            responses_req
                .as_object_mut()
                .map(|o| o.remove("previous_response_id"));
            responses_req["input"] = json!(input_items);
            let chain = ChainContext {
                key: Some(key.clone()),
                linked: false,
                input_items: input_items.to_vec(),
                props,
                est_this: estimate_request_tokens(responses_req),
                cached_est: 0,
            };
            if let WsAttempt::Streamed(resp) = ws_attempt(
                &ctx,
                &key,
                responses_req,
                original_model.to_string(),
                spec.clone(),
                chain,
            )
            .await
            {
                return Some(resp);
            }
        }
        WsAttempt::Fallback => {}
    }
    // Restore the full input for the HTTP fallback.
    responses_req
        .as_object_mut()
        .map(|o| o.remove("previous_response_id"));
    responses_req["input"] = json!(input_items);
    None
}

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
    // The model is part of the key: a conversation cannot chain across
    // models anyway (it is in `props`), and without it Claude Code's small
    // background calls would contend with the main agent for the same
    // WebSocket session and knock it out of chaining.
    let key = if is_stream {
        session_key(&headers).map(|k| format!("{k}|{}", spec.id))
    } else {
        None
    };
    let mut linked = false;
    let mut delta_len = 0usize;
    let mut baseline_len = 0usize;
    // Estimated size of the context this turn reconnects to (previous turns),
    // for the /usage cache columns.
    let mut cached_est = 0u64;
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
                        cached_est = conv.cumulative;
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

    // The model shown to the client is the one it sent (the node proxy's
    // `originalModel`), so /usage and the message id look right to it.
    let original_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Subscription mode: the ChatGPT backend rejects previous_response_id on
    // the HTTP endpoint, so chaining only works over the WebSocket channel
    // (as the codex CLI does). Try it first; on any failure the request falls
    // back to the HTTP path below with the full input restored.
    if let Some(k) = &key {
        if ctx.config.mode == "subscription" {
            if let Some(resp) = ws_handle(
                ctx.clone(),
                k.clone(),
                &mut responses_req,
                &input_items,
                props.clone(),
                linked,
                cached_est,
                &original_model,
                &spec,
                &headers,
            )
            .await
            {
                return resp;
            }
            linked = false; // ws_handle restored the full input
        }
    }

    // This is the unchained path: the whole conversation is resent every
    // turn, so the upstream prompt cache is the only thing keeping it cheap.
    // `prompt_cache_key` routes turns that share a prefix to the same cache;
    // without it the measured hit rate on this path is under 10%.
    if let Some(k) = &key {
        if ctx.send_cache_key.load(Ordering::Relaxed) {
            responses_req["prompt_cache_key"] = json!(format!("opencc-{k}"));
        }
    }

    let fetch = |req: &Value, token: &str, account_id: &Option<String>| {
        let mut builder = ctx
            .client
            .post(format!("{}/responses", ctx.config.api_base))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {token}"))
            .json(req);
        if let Some(aid) = account_id {
            builder = builder.header("ChatGPT-Account-ID", aid);
        }
        builder
    };

    // Retry without chaining: full input and no previous_response_id.
    let full_request = |req: &Value| {
        let mut full_req = req.clone();
        full_req
            .as_object_mut()
            .map(|o| o.remove("previous_response_id"));
        full_req["input"] = json!(input_items);
        full_req
    };

    let mut upstream = match fetch(&responses_req, &token, &account_id).send().await {
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
                upstream = match fetch(&responses_req, &auth.0, &auth.1).send().await {
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
    // `prompt_cache_key` is documented for the public API; the ChatGPT
    // backend is stricter about parameters it does not know. Discover that
    // once from the rejection rather than assuming either way, then stop
    // sending it for the rest of the process.
    if upstream.status() == StatusCode::BAD_REQUEST
        && responses_req.get("prompt_cache_key").is_some()
    {
        let err_text = upstream.text().await.unwrap_or_default();
        if err_text.contains("prompt_cache_key") {
            eprintln!("[opencc] upstream rejected prompt_cache_key: disabling it");
            ctx.send_cache_key.store(false, Ordering::Relaxed);
            responses_req
                .as_object_mut()
                .map(|o| o.remove("prompt_cache_key"));
            upstream = match fetch(&responses_req, &auth.0, &auth.1).send().await {
                Ok(r) => r,
                Err(err) => {
                    return json_response(
                        StatusCode::BAD_GATEWAY,
                        &json!({"error": {"message": format!("Upstream error: {err}")}}),
                    )
                    .await;
                }
            };
        } else {
            // An unrelated 400: report it as the normal error path would.
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": upstream_error_message(&err_text)}}),
            )
            .await;
        }
    }

    // Chaining can fail (e.g. response expired server-side): retry with the
    // full request and reset the conversation state.
    if linked && !upstream.status().is_success() {
        eprintln!(
            "[opencc] delta failed ({}): retrying without chaining",
            upstream.status()
        );
        upstream = match fetch(&full_request(&responses_req), &auth.0, &auth.1)
            .send()
            .await
        {
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
                    est_this: estimate_request_tokens(&responses_req),
                    cached_est: if linked { cached_est } else { 0 },
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
    /// Usage from the final response.completed/done event.
    stream_usage: Option<Usage>,
    /// Estimated usage of this request (for message_start).
    fresh_est: u64,
    /// Estimated size of the reconnected context (for message_start).
    cached_est: u64,
    /// Set when the upstream failed the response mid-stream (quota,
    /// policy): the stream is terminated by an Anthropic error event.
    failed: Option<String>,
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
            stream_usage: None,
            fresh_est: 0,
            cached_est: 0,
            failed: None,
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

/// Produces `message_start` + `ping` once, before the first content block.
fn message_start_events(state: &mut StreamState, original_model: &str) -> Vec<(String, Value)> {
    if state.started {
        return Vec::new();
    }
    state.started = true;
    // The real usage arrives only with response.completed, which is after the
    // stream: /usage reads the numbers from message_delta, but the context
    // bar wants something at start — report the request estimates.
    let start_usage = Usage {
        input_tokens: state.fresh_est,
        cache_read_input_tokens: state.cached_est,
        ..Default::default()
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
            "usage": build_usage_payload(original_model, &start_usage),
        },
    });
    vec![
        ("message_start".to_string(), msg),
        ("ping".to_string(), json!({"type": "ping"})),
    ]
}

/// Applies one upstream Responses event to the state machine and returns the
/// Anthropic SSE events it produces (message_start/ping included when the
/// stream has not started yet).
fn process_upstream_event(
    state: &mut StreamState,
    evt: &Value,
    original_model: &str,
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    match s(evt, "type") {
        Some("response.completed") | Some("response.done") => {
            let resp = evt.get("response").unwrap_or(evt);
            // /usage accumulates input/cache from the final
            // message_delta usage: the full usage is needed, not just
            // output_tokens.
            state.stream_usage = Some(extract_usage(resp.get("usage")));
            state.resp_id = resp.get("id").and_then(|v| v.as_str()).map(String::from);
        }
        Some("response.failed") => {
            let resp = evt.get("response").unwrap_or(evt);
            let message = resp
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .or_else(|| s(resp, "error"))
                .unwrap_or("Upstream request failed")
                .to_string();
            state.failed = Some(message.clone());
            out.push((
                "error".to_string(),
                json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": message},
                }),
            ));
        }
        Some("error") => {
            // Upstream error frame (e.g. quota refusal mid-stream).
            let message = evt
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("Upstream error")
                .to_string();
            state.failed = Some(message.clone());
            out.push((
                "error".to_string(),
                json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": message},
                }),
            ));
        }
        Some("response.output_item.added") => {
            if let Some(item) = evt.get("item") {
                let oi = u(evt, "output_index")
                    .or_else(|| u(item, "index"))
                    .unwrap_or(state.blocks.len());
                if s(item, "type") == Some("function_call") {
                    out.extend(message_start_events(state, original_model));
                    let idx = state.open_block(oi, "tool_use");
                    state.has_tool_use = true;
                    let call_id = s(item, "call_id").unwrap_or("").to_string();
                    let id = s(item, "id").unwrap_or("").to_string();
                    let name = s(item, "name").unwrap_or("").to_string();
                    out.push((
                        "content_block_start".to_string(),
                        json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": {
                                "type": "tool_use",
                                "id": if call_id.is_empty() { id.clone() } else { call_id.clone() },
                                "name": name,
                                "input": {},
                            },
                        }),
                    ));
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
            let oi = u(evt, "output_index").or_else(|| evt.get("item").and_then(|i| u(i, "index")));
            if let (Some(oi), Some(idx)) =
                (oi, oi.and_then(|oi| state.blocks.get(&oi)).map(|b| b.idx))
            {
                state.close_block(Some(oi));
                out.push((
                    "content_block_stop".to_string(),
                    json!({
                        "type": "content_block_stop",
                        "index": idx,
                    }),
                ));
            }
        }
        Some("response.output_text.delta") => {
            if let Some(delta) = s(evt, "delta") {
                let oi = u(evt, "output_index").unwrap_or(0);
                state
                    .resp_texts
                    .entry(oi)
                    .and_modify(|t| t.push_str(delta))
                    .or_insert_with(|| delta.to_string());
                if !state.blocks.contains_key(&oi) {
                    out.extend(message_start_events(state, original_model));
                    let idx = state.open_block(oi, "text");
                    out.push((
                        "content_block_start".to_string(),
                        json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": { "type": "text", "text": "" },
                        }),
                    ));
                }
                let idx = state.blocks.get(&oi).map(|b| b.idx).unwrap_or(0);
                out.push((
                    "content_block_delta".to_string(),
                    json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "text_delta", "text": delta },
                    }),
                ));
            }
        }
        Some("response.function_call_arguments.delta") => {
            if let Some(delta) = s(evt, "delta") {
                let oi = u(evt, "output_index").unwrap_or(0);
                if let Some(tool) = state.resp_tool_calls.get_mut(&oi) {
                    tool.args.push_str(delta);
                }
                if let Some(idx) = state.blocks.get(&oi).map(|b| b.idx) {
                    out.push((
                        "content_block_delta".to_string(),
                        json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": { "type": "input_json_delta", "partial_json": delta },
                        }),
                    ));
                }
            }
        }
        Some("response.function_call_arguments.done") => {
            let oi = u(evt, "output_index");
            if let (Some(oi), Some(idx)) =
                (oi, oi.and_then(|oi| state.blocks.get(&oi)).map(|b| b.idx))
            {
                state.close_block(Some(oi));
                out.push((
                    "content_block_stop".to_string(),
                    json!({
                        "type": "content_block_stop",
                        "index": idx,
                    }),
                ));
            }
        }
        _ => {}
    }
    out
}

type StreamSender = http_body_util::channel::Sender<Bytes>;

/// Sends the Anthropic events produced for one upstream event. Returns false
/// when the client is gone.
async fn send_translated(
    state: &mut StreamState,
    evt: &Value,
    original_model: &str,
    sender: &mut Option<StreamSender>,
) -> bool {
    for (event, data) in process_upstream_event(state, evt, original_model) {
        let Some(s) = sender.as_mut() else {
            return false;
        };
        if s.send_data(Bytes::from(format_sse(&event, &data)))
            .await
            .is_err()
        {
            *sender = None; // client gone
            return false;
        }
    }
    true
}

/// No event from the upstream for this long mid-response: the connection is
/// wedged. Failing the turn beats hanging the agent on it forever.
const WS_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Terminates the stream with an Anthropic `error` event. Used when the
/// upstream dies mid-response: without it the client receives a well-formed
/// but empty assistant turn, which reads as success.
async fn fail_stream(state: &mut StreamState, sender: &mut Option<StreamSender>, message: String) {
    eprintln!("[opencc] stream failed: {message}");
    state.failed = Some(message.clone());
    if let Some(s) = sender.as_mut() {
        let data = json!({
            "type": "error",
            "error": {"type": "api_error", "message": message},
        });
        if s.send_data(Bytes::from(format_sse("error", &data)))
            .await
            .is_err()
        {
            *sender = None; // client gone
        }
    }
}

/// State needed to remember (or forget) a conversation for turn chaining.
struct ChainContext {
    key: Option<String>,
    linked: bool,
    input_items: Vec<Value>,
    props: String,
    /// Estimated tokens of the request actually sent (delta when chained).
    est_this: u64,
    /// Estimated tokens of the reconnected context (previous turns).
    cached_est: u64,
}

/// Stream tail shared by the HTTP and WebSocket paths: closes open blocks,
/// remembers the conversation for chaining, logs the usage and emits
/// message_delta + message_stop (or the empty-response sequence).
async fn finalize_stream(
    ctx: &Shared,
    sender: &mut Option<StreamSender>,
    state: &mut StreamState,
    original_model: &str,
    spec: &ModelSpec,
    chain: ChainContext,
) {
    macro_rules! emit {
        ($event:expr, $data:expr) => {
            if let Some(s) = sender.as_mut() {
                let bytes = Bytes::from(format_sse(&$event, &$data));
                if s.send_data(bytes).await.is_err() {
                    *sender = None; // client gone
                }
            }
        };
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
        match state.resp_id.clone() {
            Some(resp_id) => ctx.conversations.remember(
                key,
                ConvState {
                    last_response_id: resp_id,
                    last_input: chain.input_items,
                    last_response_items: response_items,
                    props: chain.props,
                    cumulative: chain.cached_est + chain.est_this,
                },
            ),
            // The turn produced no response id (it failed or was cut short).
            // Whatever id we still hold points at a response the next turn
            // cannot chain onto: keeping it means the next turn chains,
            // gets rejected upstream, and pays a full resend to find out.
            // Drop it and let the next turn start a fresh chain.
            None => ctx.conversations.forget(key),
        }
    }
    let mut su = state.stream_usage.clone().unwrap_or_default();
    // The backend does not count the reconnected context on chained turns
    // (it reports cached=0): /usage would show the cache reads as zero even
    // though the whole baseline is billed at cache rates. Report our own
    // estimate of the baseline instead.
    if chain.linked {
        su.cache_read_input_tokens = su.cache_read_input_tokens.max(chain.cached_est);
    }
    eprintln!(
        "[opencc] usage {}: in={} cached={} out={} ({})",
        spec.id,
        su.input_tokens,
        su.cache_read_input_tokens,
        su.output_tokens,
        if chain.linked { "delta" } else { "full" }
    );

    if state.failed.is_some() {
        // The Anthropic error event already terminated the stream.
        return;
    }

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
        for (event, data) in message_start_events(state, original_model) {
            emit!(event, data);
        }
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
}

/// Writes the upstream Responses SSE stream to the client as Anthropic SSE.
/// On client abort the upstream body is dropped.
async fn stream_translation(
    ctx: Shared,
    upstream: reqwest::Response,
    sender: StreamSender,
    original_model: &str,
    spec: &ModelSpec,
    chain: ChainContext,
) {
    let mut state = StreamState::new();
    state.fresh_est = chain.est_this;
    state.cached_est = chain.cached_est;
    let mut parser = SseParser::new();
    let mut sender: Option<StreamSender> = Some(sender);

    let mut chunks = upstream.bytes_stream();
    // Set when the body ended early: a transport error, or a stream that
    // stopped before response.completed. Reported as an error event rather
    // than as an empty assistant turn.
    let mut broken: Option<String> = None;
    'outer: loop {
        let chunk = match chunks.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(err)) => {
                broken = Some(format!("upstream stream error: {err}"));
                break;
            }
            None => break,
        };
        for line in parser.feed(&chunk) {
            if sender.is_none() {
                break 'outer;
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
            if !send_translated(&mut state, &evt, original_model, &mut sender).await {
                break 'outer;
            }
        }
        if sender.is_none() {
            break;
        }
    }

    // A body that ended without response.completed is a failed turn, not an
    // empty one. Saying so lets Claude Code surface the failure instead of
    // silently retrying against an assistant turn that produced nothing.
    if sender.is_some() && state.failed.is_none() && state.resp_id.is_none() {
        let reason = broken.unwrap_or_else(|| {
            "the upstream ended the stream before completing the response".to_string()
        });
        fail_stream(&mut state, &mut sender, reason).await;
    }
    finalize_stream(&ctx, &mut sender, &mut state, original_model, spec, chain).await;
    drop(sender); // end of the stream body
}

/// WebSocket variant of [`stream_translation`]: consumes the events of one
/// response from the shared connection. The receiver is returned to the
/// session on success; on client abort the session is killed so the next
/// request reconnects with a full resend.
#[allow(clippy::too_many_arguments)] // internal plumbing: stream context
async fn ws_stream_translation(
    ctx: Shared,
    key: String,
    gen: u64,
    first: Value,
    mut rx: mpsc::Receiver<Value>,
    sender: StreamSender,
    original_model: &str,
    spec: &ModelSpec,
    chain: ChainContext,
) {
    let mut state = StreamState::new();
    state.fresh_est = chain.est_this;
    state.cached_est = chain.cached_est;
    let mut sender: Option<StreamSender> = Some(sender);
    let mut pending: Option<Value> = Some(first);
    let mut aborted = false;
    // Set when the response ended without a terminal event: the socket died
    // mid-response, or went quiet long enough to be considered wedged. Either
    // way the connection is unusable and must not go back into the pool.
    let mut broken: Option<String> = None;
    let mut completed = false;
    loop {
        if sender.is_none() {
            aborted = true;
            break;
        }
        let evt = match pending.take() {
            Some(v) => v,
            None => match tokio::time::timeout(WS_STREAM_IDLE_TIMEOUT, rx.recv()).await {
                Ok(Some(v)) => v,
                // The reader task ended: the upstream closed the socket
                // before completing the response.
                Ok(None) => {
                    broken = Some(
                        "the upstream closed the connection before completing the response"
                            .to_string(),
                    );
                    break;
                }
                Err(_) => {
                    broken = Some(format!(
                        "no response from the upstream for {}s",
                        WS_STREAM_IDLE_TIMEOUT.as_secs()
                    ));
                    break;
                }
            },
        };
        // The connection stays open for the next request: stop consuming at
        // the end of this response.
        let terminal = matches!(
            s(&evt, "type"),
            Some("response.completed")
                | Some("response.done")
                | Some("response.failed")
                | Some("error")
        );
        if !send_translated(&mut state, &evt, original_model, &mut sender).await {
            aborted = true;
            break;
        }
        if terminal {
            completed = true;
            break;
        }
    }
    if let Some(reason) = broken {
        fail_stream(&mut state, &mut sender, reason).await;
    }
    finalize_stream(&ctx, &mut sender, &mut state, original_model, spec, chain).await;
    // Only a connection that carried a complete response goes back into the
    // pool. Returning a dead receiver here is what used to hand the next turn
    // a corpse: it would write into a closed socket, wait out the first-event
    // timeout, and pay a full resend to discover the connection was gone.
    if completed && !aborted {
        ctx.ws_sessions.release(&key, gen, rx);
    } else {
        ctx.ws_sessions.kill(&key, gen);
        ctx.conversations.forget(&key);
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
    fn estimates_request_tokens() {
        let req = json!({
            "model": "gpt-two",
            "instructions": "You are a helpful assistant.",
            "input": [{"role": "user", "content": "hello"}],
            "store": false,
            "stream": true,
        });
        let est = estimate_request_tokens(&req);
        // ~160 chars → ~40 tokens; must be nonzero and scale with size.
        assert!(est > 0);
        let big = json!({"input": [{"role": "user", "content": "x".repeat(10000)}]});
        let big_est = estimate_request_tokens(&big);
        assert!(big_est > est * 10);
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
