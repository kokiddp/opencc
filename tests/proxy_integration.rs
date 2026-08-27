//! End-to-end tests of the proxy: the real `opencc-proxy` binary is spawned
//! and talked to over HTTP, with the upstream OpenAI/opencode endpoint mocked
//! by a local hyper server. Port of the node test suite
//! (opencc-proxy.test.mjs).

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request as HttpRequest, Response as HttpResponse, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

const PROXY_BIN: &str = env!("CARGO_BIN_EXE_opencc-proxy");

// ── Mock upstream server ───────────────────────────────────────────────────────

struct MockRequest {
    #[allow(dead_code)] // recorded for assertions
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl MockRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

struct MockResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl MockResponse {
    fn json(status: u16, body: &Value) -> Self {
        MockResponse {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_string(body).unwrap().into_bytes(),
        }
    }
    fn sse(status: u16, events: &[&str]) -> Self {
        MockResponse {
            status,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: events.join("\n").into_bytes(),
        }
    }
}

struct MockServer {
    port: u16,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    _thread: std::thread::JoinHandle<()>,
}

/// Starts a mock HTTP server on a random port. Every request is recorded in
/// `requests` (in order) and answered by `handler`.
fn start_mock<F>(handler: F) -> MockServer
where
    F: Fn(&MockRequest) -> MockResponse + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let requests: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let port = Arc::new(Mutex::new(None::<u16>));

    let handler2 = handler.clone();
    let requests2 = requests.clone();
    let port2 = port.clone();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            *port2.lock().unwrap() = Some(listener.local_addr().unwrap().port());
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let handler = handler2.clone();
                let requests = requests2.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req: HttpRequest<hyper::body::Incoming>| {
                        let handler = handler.clone();
                        let requests = requests.clone();
                        async move {
                            let method = req.method().to_string();
                            let path = req.uri().path().to_string();
                            let headers: Vec<(String, String)> = req
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())
                                })
                                .collect();
                            let body = match req.into_body().collect().await {
                                Ok(c) => c.to_bytes(),
                                Err(_) => Bytes::new(),
                            };
                            requests.lock().unwrap().push(MockRequest {
                                method,
                                path,
                                headers,
                                body,
                            });
                            let resp = handler(requests.lock().unwrap().last().unwrap());
                            let mut b = HttpResponse::new(Full::new(Bytes::from(resp.body)));
                            *b.status_mut() = StatusCode::from_u16(resp.status)
                                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                            for (k, v) in resp.headers {
                                if let (Ok(name), Ok(value)) = (
                                    hyper::header::HeaderName::from_bytes(k.as_bytes()),
                                    hyper::header::HeaderValue::from_str(&v),
                                ) {
                                    b.headers_mut().insert(name, value);
                                }
                            }
                            Ok::<_, std::convert::Infallible>(b)
                        }
                    });
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });
    });

    // Wait for the port.
    for _ in 0..200 {
        if let Some(p) = *port.lock().unwrap() {
            return MockServer {
                port: p,
                requests,
                _thread: thread,
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("mock server did not start");
}

// ── Proxy fixture ──────────────────────────────────────────────────────────────

struct ProxyFixture {
    port: u16,
    child: Child,
}

impl Drop for ProxyFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn_proxy(extra_env: &[(&str, String)]) -> ProxyFixture {
    let port = free_port();
    let mut cmd = Command::new(PROXY_BIN);
    // Scrub everything the proxy reads, so the developer's environment
    // (real API keys, OPENCC_* settings) cannot leak into the tests.
    for key in [
        "OPENCC_MODE",
        "OPENCC_PROXY_PORT",
        "OPENCC_MODELS",
        "OPENCC_FALLBACK_MODEL",
        "OPENCC_EFFORT_POLICY_FILE",
        "OPENCC_GO_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_API_BASE",
        "OPENAI_AUTH_BASE",
        "CHATGPT_API_BASE",
        "OPENCODE_API_KEY",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("OPENCC_PROXY_PORT", port.to_string())
        .env("OPENCC_MODELS", "gpt-one,gpt-two")
        .envs(extra_env.iter().map(|(k, v)| (*k, v.clone())))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("proxy spawns");

    // Wait for /health.
    let client = reqwest::blocking::Client::new();
    let mut child = child;
    for _ in 0..200 {
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(std::time::Duration::from_millis(200))
            .send()
        {
            if resp.status().is_success() {
                return ProxyFixture { port, child };
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        if let Ok(Some(_)) = child.try_wait() {
            panic!("proxy exited during startup");
        }
    }
    panic!("proxy did not become healthy");
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::new()
}

/// The effort policy file shared by the tests (same models as the node suite).
fn write_policy_file() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("opencc-it-policy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("model-efforts.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "models": {
                "gpt-one": {"supported": [], "default": null},
                "gpt-two": {"supported": ["low", "medium", "high"], "default": "medium"},
            }
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

fn home_env(home: &std::path::Path) -> Vec<(&'static str, String)> {
    #[cfg(unix)]
    {
        vec![("HOME", home.to_string_lossy().into_owned())]
    }
    #[cfg(windows)]
    {
        vec![
            ("USERPROFILE", home.to_string_lossy().into_owned()),
            ("HOMEDRIVE", home.to_string_lossy().into_owned()),
            ("HOMEPATH", "".to_string()),
        ]
    }
}

/// Parses a raw SSE document into (event, data) pairs.
fn parse_sse(text: &str) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    for block in text.split("\n\n") {
        let mut event = String::new();
        let mut data = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event: ") {
                event = v.to_string();
            } else if let Some(v) = line.strip_prefix("data: ") {
                data.push_str(v);
            }
        }
        if !data.is_empty() {
            events.push((event, serde_json::from_str(&data).unwrap_or(Value::Null)));
        }
    }
    events
}

fn event_types(events: &[(String, Value)]) -> Vec<&str> {
    events.iter().map(|(e, _)| e.as_str()).collect()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[test]
fn health_and_models_are_exposed() {
    let fixture = spawn_proxy(&[
        ("OPENCC_MODE", "apikey".into()),
        ("OPENAI_API_KEY", "test-key".into()),
        ("OPENCC_FALLBACK_MODEL", "gpt-fallback@high".into()),
    ]);
    let c = client();

    let health: Value = c
        .get(format!("http://127.0.0.1:{}/health", fixture.port))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["mode"], "apikey");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(health["fallback"], "gpt-fallback@high");

    let models: Value = c
        .get(format!("http://127.0.0.1:{}/v1/models", fixture.port))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["gpt-one", "gpt-two"]);
}

#[test]
fn non_stream_messages_translate_with_effort_policy() {
    let policy = write_policy_file();
    let mock = start_mock(|_req| {
        MockResponse::sse(
            200,
            &[
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}",
                "data: [DONE]",
                "",
            ],
        )
    });
    let fixture = spawn_proxy(&[
        ("OPENCC_MODE", "apikey".into()),
        ("OPENAI_API_KEY", "test-key".into()),
        ("OPENAI_API_BASE", format!("http://127.0.0.1:{}", mock.port)),
        (
            "OPENCC_EFFORT_POLICY_FILE",
            policy.to_string_lossy().into_owned(),
        ),
    ]);
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);

    // gpt-two supports medium: the effort passes through.
    let resp = c
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "gpt-two",
            "messages": [{"role": "user", "content": "hello"}],
            "output_config": {"effort": "medium"},
            "stream": false,
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["content"][0]["text"], "ok");
    assert_eq!(body["model"], "gpt-two");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 1);

    let upstream = mock.requests.lock().unwrap();
    assert_eq!(upstream[0].path, "/responses");
    assert_eq!(upstream[0].header("authorization"), Some("Bearer test-key"));
    let sent = upstream[0].json();
    assert_eq!(sent["model"], "gpt-two");
    assert_eq!(sent["reasoning"], json!({"effort": "medium"}));
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["store"], false);
    drop(upstream);

    // gpt-one has no reasoning: the effort is removed entirely.
    let resp = c
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "gpt-one",
            "messages": [{"role": "user", "content": "hello"}],
            "output_config": {"effort": "max"},
            "stream": false,
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let upstream = mock.requests.lock().unwrap();
    assert_eq!(upstream[1].json()["model"], "gpt-one");
    assert!(upstream[1].json().get("reasoning").is_none());
}

#[test]
fn streaming_sse_sequence_with_text_and_tools() {
    let policy = write_policy_file();
    let mock = start_mock(|_req| {
        MockResponse::sse(
            200,
            &[
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\",\"id\":\"fc_1\"}}",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"command\\\":\\\"ls\"}",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"}\"}",
                "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\",\"call_id\":\"call_1\",\"name\":\"bash\"}",
                "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\",\"content\":[]}}",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"and the \"}",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"result\"}",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":1}",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"usage\":{\"input_tokens\":50,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":20}}}}",
                "data: [DONE]",
                "",
            ],
        )
    });
    let fixture = spawn_proxy(&[
        ("OPENCC_MODE", "apikey".into()),
        ("OPENAI_API_KEY", "test-key".into()),
        ("OPENAI_API_BASE", format!("http://127.0.0.1:{}", mock.port)),
        (
            "OPENCC_EFFORT_POLICY_FILE",
            policy.to_string_lossy().into_owned(),
        ),
    ]);
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);

    let resp = c
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "gpt-two",
            "messages": [{"role": "user", "content": "run ls"}],
            "stream": true,
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let text = resp.text().unwrap();
    let events = parse_sse(&text);

    // The tool-use block opens first, its JSON args stream as partial_json,
    // then the text block streams, and the usage is the full converted one.
    assert_eq!(
        event_types(&events),
        vec![
            "message_start",
            "ping",
            "content_block_start", // tool_use
            "content_block_delta", // input_json_delta
            "content_block_delta", // input_json_delta
            "content_block_stop",
            "content_block_start", // text
            "content_block_delta", // text_delta
            "content_block_delta", // text_delta
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    let blocks: Vec<&Value> = events
        .iter()
        .filter(|(e, _)| e == "content_block_start")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(blocks[0]["content_block"]["type"], "tool_use");
    assert_eq!(blocks[0]["content_block"]["name"], "bash");
    assert_eq!(blocks[0]["content_block"]["id"], "call_1");
    assert_eq!(blocks[1]["content_block"]["type"], "text");
    assert_eq!(blocks[1]["index"], 1);

    let deltas: Vec<&Value> = events
        .iter()
        .filter(|(e, _)| e == "content_block_delta")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(deltas[0]["delta"]["type"], "input_json_delta");
    assert_eq!(deltas[0]["delta"]["partial_json"], "{\"command\":\"ls");
    assert_eq!(deltas[1]["delta"]["partial_json"], "\"}");
    assert_eq!(deltas[2]["delta"]["type"], "text_delta");
    assert_eq!(deltas[2]["delta"]["text"], "and the ");
    assert_eq!(deltas[3]["delta"]["text"], "result");

    let msg_delta = events
        .iter()
        .find(|(e, _)| e == "message_delta")
        .unwrap()
        .1
        .clone();
    assert_eq!(msg_delta["delta"]["stop_reason"], "tool_use");
    // Cached tokens are split out of input_tokens for /usage; the cache
    // columns live in current_usage (like the node proxy).
    assert_eq!(msg_delta["usage"]["input_tokens"], 30);
    assert_eq!(msg_delta["usage"]["output_tokens"], 7);
    assert_eq!(
        msg_delta["usage"]["current_usage"]["cache_read_input_tokens"],
        20
    );
    assert_eq!(msg_delta["usage"]["total_input_tokens"], 50);
}

#[test]
fn expired_oauth_token_is_refreshed_and_auth_json_rewritten() {
    // Mock auth endpoint.
    let auth_calls = Arc::new(Mutex::new(Vec::new()));
    let auth_calls2 = auth_calls.clone();
    let auth = start_mock(move |req: &MockRequest| {
        auth_calls2.lock().unwrap().push(req.json());
        MockResponse::json(
            200,
            &json!({
                "access_token": "new-token",
                "refresh_token": "new-refresh",
                "account_id": "acc-2",
                "id_token": "id-2",
            }),
        )
    });

    // Mock upstream: 401 on the first call (old token), 200 on the second.
    let calls = Arc::new(Mutex::new(0usize));
    let calls2 = calls.clone();
    let upstream = start_mock(move |req: &MockRequest| {
        let n = {
            let mut c = calls2.lock().unwrap();
            *c += 1;
            *c
        };
        if n == 1 {
            assert_eq!(req.header("authorization"), Some("Bearer old-token"));
            MockResponse::json(401, &json!({"error": {"message": "token expired"}}))
        } else {
            assert_eq!(req.header("authorization"), Some("Bearer new-token"));
            MockResponse::sse(
                200,
                &[
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fresh\"}",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_3\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}",
                    "",
                ],
            )
        }
    });

    // A temp HOME with a (fake) codex auth.json.
    let home = std::env::temp_dir().join(format!("opencc-it-home-{}", std::process::id()));
    let codex = home.join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    let auth_json = codex.join("auth.json");
    std::fs::write(
        &auth_json,
        serde_json::to_string(&json!({
            "tokens": {
                "access_token": "old-token",
                "refresh_token": "rt-1",
                "account_id": "acc-1",
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let mut envs = home_env(&home);
    envs.push(("OPENCC_MODE", "subscription".into()));
    envs.push((
        "CHATGPT_API_BASE",
        format!("http://127.0.0.1:{}", upstream.port),
    ));
    envs.push((
        "OPENAI_AUTH_BASE",
        format!("http://127.0.0.1:{}/oauth/token", auth.port),
    ));
    let fixture = spawn_proxy(&envs);
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);

    let resp = c
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "gpt-one",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": false,
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["content"][0]["text"], "fresh");

    // The refresh was called once with the old refresh_token.
    let calls = auth_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["grant_type"], "refresh_token");
    assert_eq!(calls[0]["refresh_token"], "rt-1");
    assert_eq!(calls[0]["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
    drop(calls);

    // auth.json was rewritten with the new tokens.
    let rewritten: Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_json).unwrap()).unwrap();
    assert_eq!(rewritten["tokens"]["access_token"], "new-token");
    assert_eq!(rewritten["tokens"]["refresh_token"], "new-refresh");
    assert_eq!(rewritten["tokens"]["account_id"], "acc-2");
    assert!(rewritten.get("last_refresh").is_some());

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn opencode_pass_through_normalizes_and_forwards_headers() {
    let policy = write_policy_file();
    let mock = start_mock(|_req| {
        MockResponse {
            status: 200,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("content-encoding".into(), "br".into()), // must be stripped
                ("x-custom".into(), "kept".into()),
            ],
            body: br#"{"id":"msg_go","type":"message","role":"assistant","model":"gpt-two","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec(),
        }
    });
    let fixture = spawn_proxy(&[
        ("OPENCC_MODE", "opencode".into()),
        ("OPENCODE_API_KEY", "go-test-key".into()),
        (
            "OPENCC_GO_BASE_URL",
            format!("http://127.0.0.1:{}", mock.port),
        ),
        (
            "OPENCC_EFFORT_POLICY_FILE",
            policy.to_string_lossy().into_owned(),
        ),
    ]);
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);

    let resp = c
        .post(format!("{base}/v1/messages?beta=true"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "test-beta")
        .json(&json!({
            "model": "gpt-two",
            "messages": [{"role": "user", "content": "hello"}],
            "output_config": {"effort": "max", "format": {"type": "json_schema"}},
            "stream": false,
        }))
        .send()
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-custom"], "kept");
    assert!(
        resp.headers().get("content-encoding").is_none(),
        "content-encoding must be stripped"
    );
    let body: Value = resp.json().unwrap();
    assert_eq!(body["id"], "msg_go");
    assert_eq!(body["content"][0]["text"], "ok");

    let upstream = mock.requests.lock().unwrap();
    assert_eq!(upstream.len(), 1);
    assert_eq!(upstream[0].path, "/v1/messages");
    assert_eq!(upstream[0].header("x-api-key"), Some("go-test-key"));
    assert_eq!(upstream[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(upstream[0].header("anthropic-beta"), Some("test-beta"));
    assert_eq!(upstream[0].header("accept-encoding"), Some("identity"));
    // The effort was clamped max → high per the policy; the rest passes through.
    assert_eq!(upstream[0].json()["model"], "gpt-two");
    assert_eq!(
        upstream[0].json()["output_config"],
        json!({"format": {"type": "json_schema"}, "effort": "high"})
    );
}

#[test]
fn unknown_model_is_rejected_and_missing_key_is_401() {
    // apikey mode with an unhandled model (no fallback set).
    let fixture = spawn_proxy(&[
        ("OPENCC_MODE", "apikey".into()),
        ("OPENAI_API_KEY", "test-key".into()),
    ]);
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);
    let resp = c
        .post(format!("{base}/v1/messages"))
        .json(&json!({"model": "claude-opus-4-1", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("OPENCC_FALLBACK_MODEL"));

    // opencode mode without a key → 401.
    let fixture = spawn_proxy(&[("OPENCC_MODE", "opencode".into())]);
    let base = format!("http://127.0.0.1:{}", fixture.port);
    let resp = c
        .post(format!("{base}/v1/messages"))
        .json(&json!({"model": "gpt-one", "messages": []}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[test]
fn chaining_sends_the_delta_and_falls_back_on_failure() {
    let policy = write_policy_file();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let requests2 = requests.clone();
    let mock = start_mock(move |req: &MockRequest| {
        requests2.lock().unwrap().push(req.json());
        let n = requests2.lock().unwrap().len();
        if n == 1 {
            MockResponse::sse(
                200,
                &[
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_chain_1\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}",
                    "",
                ],
            )
        } else {
            MockResponse::sse(
                200,
                &[
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"second\"}",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_chain_2\",\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}}",
                    "",
                ],
            )
        }
    });
    let fixture = spawn_proxy(&[
        ("OPENCC_MODE", "apikey".into()),
        ("OPENAI_API_KEY", "test-key".into()),
        ("OPENAI_API_BASE", format!("http://127.0.0.1:{}", mock.port)),
        (
            "OPENCC_EFFORT_POLICY_FILE",
            policy.to_string_lossy().into_owned(),
        ),
    ]);
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);
    let session = format!("sess-{}", std::process::id());

    let send = |messages: Value| {
        c.post(format!("{base}/v1/messages"))
            .header("x-claude-code-session-id", session.as_str())
            .json(&json!({"model": "gpt-two", "messages": messages, "stream": true}))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };

    // Turn 1: full input.
    let first = send(json!([{"role": "user", "content": "hello"}]));
    assert!(first.contains("first"));
    // Turn 2: extension → only the delta + previous_response_id.
    let second = send(json!([
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "first"},
        {"role": "user", "content": "and then?"},
    ]));
    assert!(second.contains("second"));

    let sent = requests.lock().unwrap();
    assert_eq!(sent.len(), 2);
    // Turn 1: full input of 1 item, no chaining.
    assert_eq!(sent[0]["input"].as_array().unwrap().len(), 1);
    assert!(sent[0].get("previous_response_id").is_none());
    // Turn 2: only the new user message goes up, with the chain id
    // (the baseline = turn 1's input + the assistant's response items).
    assert_eq!(sent[1]["input"].as_array().unwrap().len(), 1);
    assert_eq!(
        sent[1]["input"][0],
        json!({"role": "user", "content": "and then?"})
    );
    assert_eq!(sent[1]["previous_response_id"], "resp_chain_1");
    drop(sent);
}

// ── Mock WebSocket upstream (subscription mode) ───────────────────────────────

/// A WebSocket server that records every client frame and answers each with
/// the frames returned by `handler` (per connection, in order).
struct WsMockServer {
    port: u16,
    frames: Arc<Mutex<Vec<Value>>>,
    connections: Arc<Mutex<usize>>,
    _thread: std::thread::JoinHandle<()>,
}

fn start_ws_mock<F>(handler: F) -> WsMockServer
where
    F: Fn(&Value, usize) -> Vec<Value> + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let connections: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let port: Arc<Mutex<Option<u16>>> = Arc::new(Mutex::new(None));

    let handler2 = handler.clone();
    let frames2 = frames.clone();
    let connections2 = connections.clone();
    let port2 = port.clone();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            *port2.lock().unwrap() = Some(listener.local_addr().unwrap().port());
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                let handler = handler2.clone();
                let frames = frames2.clone();
                let connections = connections2.clone();
                tokio::spawn(async move {
                    let ws = match tokio_tungstenite::accept_async(stream).await {
                        Ok(w) => w,
                        Err(_) => return,
                    };
                    let conn_idx = {
                        let mut c = connections.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    let (mut sink, mut stream) = ws.split();
                    while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                        stream.next().await
                    {
                        let Ok(evt) = serde_json::from_str::<Value>(text.as_str()) else {
                            continue;
                        };
                        frames.lock().unwrap().push(evt.clone());
                        for resp in handler(&evt, conn_idx) {
                            if sink
                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                    resp.to_string().into(),
                                ))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                });
            }
        });
    });
    for _ in 0..200 {
        if let Some(p) = *port.lock().unwrap() {
            return WsMockServer {
                port: p,
                frames,
                connections,
                _thread: thread,
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("ws mock server did not start");
}

/// A temp HOME with a fake codex auth.json (access_token "ws-token").
fn ws_test_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("opencc-ws-home-{}-{tag}", std::process::id()));
    let codex = home.join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("auth.json"),
        serde_json::to_string(&json!({
            "tokens": {
                "access_token": "ws-token",
                "refresh_token": "rt-1",
                "account_id": "acc-1",
            }
        }))
        .unwrap(),
    )
    .unwrap();
    home
}

fn ws_proxy_env(home: &std::path::Path, upstream_port: u16) -> Vec<(&'static str, String)> {
    let mut envs = home_env(home);
    envs.push(("OPENCC_MODE", "subscription".into()));
    envs.push((
        "CHATGPT_API_BASE",
        format!("http://127.0.0.1:{upstream_port}"),
    ));
    envs
}

#[test]
fn subscription_chains_turns_over_websocket() {
    let mock = start_ws_mock(move |frame: &Value, _conn: usize| {
        if frame.get("previous_response_id").is_some() {
            vec![
                json!({"type": "response.output_text.delta", "delta": "second"}),
                json!({"type": "response.completed", "response": {"id": "resp_ws_2", "usage": {"input_tokens": 4, "output_tokens": 1}}}),
            ]
        } else {
            vec![
                json!({"type": "response.output_text.delta", "delta": "first"}),
                json!({"type": "response.completed", "response": {"id": "resp_ws_1", "usage": {"input_tokens": 3, "output_tokens": 1}}}),
            ]
        }
    });
    let home = ws_test_home("chain");
    let fixture = spawn_proxy(&ws_proxy_env(&home, mock.port));
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);
    let session = format!("sess-{}", std::process::id());
    let send = |messages: Value| {
        c.post(format!("{base}/v1/messages"))
            .header("x-claude-code-session-id", session.as_str())
            .json(&json!({"model": "gpt-two", "messages": messages, "stream": true}))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };

    let first = send(json!([{"role": "user", "content": "hello"}]));
    assert!(first.contains("first"));
    let second = send(json!([
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "first"},
        {"role": "user", "content": "and then?"},
    ]));
    assert!(second.contains("second"));

    // /usage reads the final numbers from message_delta: turn 1 reports the
    // real input (3) with no cache; the chained turn 2 keeps the real delta
    // input (4) and reports the reconnected baseline as cache read.
    let first_events = parse_sse(&first);
    let first_delta = first_events
        .iter()
        .find(|(e, _)| e == "message_delta")
        .map(|(_, d)| d.clone())
        .unwrap();
    assert_eq!(first_delta["usage"]["input_tokens"], 3);
    assert_eq!(
        first_delta["usage"]["current_usage"]["cache_read_input_tokens"],
        0
    );

    let second_events = parse_sse(&second);
    let second_delta = second_events
        .iter()
        .find(|(e, _)| e == "message_delta")
        .map(|(_, d)| d.clone())
        .unwrap();
    assert_eq!(second_delta["usage"]["input_tokens"], 4);
    let cache_read = second_delta["usage"]["current_usage"]["cache_read_input_tokens"]
        .as_u64()
        .unwrap();
    assert!(
        cache_read > 0,
        "chained turn must report the reconnected baseline as cache read"
    );

    let frames = mock.frames.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        *mock.connections.lock().unwrap(),
        1,
        "one connection reused"
    );
    assert_eq!(frames[0]["type"], "response.create");
    assert_eq!(frames[0]["input"].as_array().unwrap().len(), 1);
    assert!(frames[0].get("previous_response_id").is_none());
    // Turn 2: only the delta goes up, with the chain id.
    assert_eq!(frames[1]["previous_response_id"], "resp_ws_1");
    assert_eq!(frames[1]["input"].as_array().unwrap().len(), 1);
    assert_eq!(
        frames[1]["input"][0],
        json!({"role": "user", "content": "and then?"})
    );
    drop(frames);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn subscription_ws_error_reconnects_with_the_full_input() {
    let mock = start_ws_mock(move |frame: &Value, conn: usize| {
        let chained = frame.get("previous_response_id").is_some();
        if conn == 1 && chained {
            // The backend rejects the stale chain: error frame.
            vec![json!({
                "type": "error", "status": 400,
                "error": {"type": "invalid_request_error", "message": "Invalid `previous_response_id`."}
            })]
        } else if conn == 1 {
            vec![
                json!({"type": "response.output_text.delta", "delta": "first"}),
                json!({"type": "response.completed", "response": {"id": "resp_ws_1", "usage": {"input_tokens": 3, "output_tokens": 1}}}),
            ]
        } else {
            vec![
                json!({"type": "response.output_text.delta", "delta": "second"}),
                json!({"type": "response.completed", "response": {"id": "resp_ws_2", "usage": {"input_tokens": 4, "output_tokens": 1}}}),
            ]
        }
    });
    let home = ws_test_home("error");
    let fixture = spawn_proxy(&ws_proxy_env(&home, mock.port));
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);
    let session = format!("sess-{}", std::process::id());
    let send = |messages: Value| {
        c.post(format!("{base}/v1/messages"))
            .header("x-claude-code-session-id", session.as_str())
            .json(&json!({"model": "gpt-two", "messages": messages, "stream": true}))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };

    let first = send(json!([{"role": "user", "content": "hello"}]));
    assert!(first.contains("first"));
    // The chained attempt fails; the proxy reconnects and resends the full
    // input, so the client still sees a complete stream.
    let second = send(json!([
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "first"},
        {"role": "user", "content": "and then?"},
    ]));
    assert!(second.contains("second"));

    let frames = mock.frames.lock().unwrap();
    assert_eq!(frames.len(), 3);
    assert_eq!(*mock.connections.lock().unwrap(), 2, "reconnected");
    assert!(frames[1].get("previous_response_id").is_some());
    // The retry sends the FULL input (3 items), no chaining.
    assert_eq!(frames[2]["input"].as_array().unwrap().len(), 3);
    assert!(frames[2].get("previous_response_id").is_none());
    drop(frames);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn subscription_falls_back_to_http_when_websocket_is_unavailable() {
    // The upstream mock only speaks HTTP: the WS upgrade fails, so the proxy
    // must fall back to a plain HTTP full request (no chaining attempts).
    let mock = start_mock(|_req| {
        MockResponse::sse(
            200,
            &[
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"via-http\"}",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http_1\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}",
                "",
            ],
        )
    });
    let home = ws_test_home("fallback");
    let fixture = spawn_proxy(&ws_proxy_env(&home, mock.port));
    let c = client();
    let base = format!("http://127.0.0.1:{}", fixture.port);
    let session = format!("sess-{}", std::process::id());

    let resp = c
        .post(format!("{base}/v1/messages"))
        .header("x-claude-code-session-id", session.as_str())
        .json(&json!({
            "model": "gpt-two",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true,
        }))
        .send()
        .unwrap();
    let text = resp.text().unwrap();
    assert!(text.contains("via-http"));

    let requests = mock.requests.lock().unwrap();
    // The WS upgrade attempt is recorded too; only the real call counts.
    let responses_calls: Vec<&MockRequest> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/responses")
        .collect();
    assert_eq!(responses_calls.len(), 1);
    assert!(responses_calls[0]
        .json()
        .get("previous_response_id")
        .is_none());
    drop(requests);
    let _ = std::fs::remove_dir_all(&home);
}
