# Plan: Rust rewrite of opencc (opencc + opencc-proxy), rename `go` → `opencode`

## Context

`opencc` is a wrapper that runs **Claude Code** against alternative backends. Today it is a bash script (`opencc`, ~660 lines) plus a Node.js HTTP proxy (`opencc-proxy.mjs`, ~1050 lines) that translates the Anthropic protocol into OpenAI's Responses API. This rewrite:

1. Rewrites `opencc` **and** `opencc-proxy` in **Rust** (removes the Node dependency entirely).
2. Targets **multiplatform/multiarch**: Linux, Windows, macOS × x86, x86_64, aarch64 (mac silicon), armv7, armv6 (Raspberry Pi 0/1), riscv64.
3. Renames the `go` backend to **`opencode`** (more accurate; the gateway is opencode-go from opencode.ai).
4. Adds **semver versioning**: `opencc --version` / `opencc -v` prints it (first release: **0.1.0**).
5. Adds **GitHub Actions**: self-running tests + **draft releases on push to master**.

The Rust rewrite also removes the `curl` + `python3` + `node` prerequisites (only `claude` remains mandatory; `codex`/`opencode` for logins).

Repo: `/home/gcoquillard/opencc`, branch `feature/rust-rewrite`, remote `github.com/kokiddp/opencc`.

## Architecture

Single Cargo crate `opencc`, version **0.1.0**, edition 2021, two binaries sharing a lib:

```
Cargo.toml                     version = "0.1.0"
src/lib.rs                     shared modules
src/main.rs                    bin "opencc" — the wrapper CLI (mostly sync)
src/bin/opencc-proxy.rs        bin "opencc-proxy" — thin: parse env, start hyper server
tests/proxy_integration.rs     mock upstream + real proxy binary (CARGO_BIN_EXE)
.github/workflows/ci.yml
.github/workflows/release.yml
install.sh                     (updated: installs the two binaries)
README.md                      (updated)
```

lib modules: `state` (platform paths, atomic write, session registry, process-alive), `models` (fetch + TSV cache), `effort` (policy + normalization), `picker` (model-picker.json / model-efforts.json), `proxy` (HTTP server, translation, SSE, turn chaining, OAuth refresh), `menus` (testable menu prompts). Crate is private (name collides with crates.io `opencc` — do not publish).

### Dependencies

`clap` (derive), `serde`/`serde_json`, `which`, `reqwest` (`default-features = false, features = ["rustls-tls", "http2", "stream", "blocking"]` — no decompression features: we send `Accept-Encoding: identity` upstream, mirroring the node proxy), `tokio`, `hyper` 1.x + `hyper-util` 0.1.x (server-auto, tokio), `futures-util`, `libc` (unix: kill-0, setsid, exit codes), `windows-sys` (windows: OpenProcess alive-check, MoveFileExW atomic rename), `base64` (URL_SAFE_NO_PAD JWT decode). No openssl.

## 1. Shared lib

- **`state.rs`**: state root = `$XDG_STATE_HOME || ~/.local/state` on unix (matches bash layout), `%LOCALAPPDATA%\opencc` on Windows. `~/.codex/auth.json`, `~/.local/share/opencode/auth.json` (mirror unix layout via `dirs::home_dir`). Atomic write: tmp + rename (unix, 0600), tmp + MoveFileExW REPLACE_EXISTING (windows). Session registry: `sessions/<pid>.sess` containing `port|mode`; sweep with `kill(pid, 0)` / `OpenProcess`; kill proxy via `kill` / `taskkill /PID`.
- **`models.rs`**: `Model { slug, display, context, efforts, default }`; TSV format identical to bash (`slug<TAB>name<TAB>context<TAB>efforts<TAB>default`). Fetchers: opencode → `/v1/models` (x-api-key) + catalog `https://models.opencode.ai/api.json` (provider key `opencode-go`, intersect efforts with Claude's `{low,medium,high,xhigh,max}`); openai subscription → parse `~/.codex/models_cache.json` (max_context_window × effective_context_window_percent); openai apikey → `api.openai.com/v1/models` filtered `^gpt-|^o[0-9]`; static fallback lists (same as bash). Cache `models.tsv` + `models.ids` with 7-day max age; background refresh in a std thread (bash's `& disown`). `fmt_ctx` (828K / 1M).
- **`effort.rs`**: `EFFORT_ORDER = [low, medium, high, xhigh, max, ultra]`; `parse_model_spec("m@ultra")`; `normalize_effort(requested, policy)` with reasons `exact|clamped|default|unknown-level|unsupported-model|no-effort|no-policy` — direct port of the node logic including clamp-down-to-highest-below.
- **`picker.rs`**: generate `model-picker.json` (`modelPicker.replaceBuiltInOptions` + options with effort descriptions) and `model-efforts.json` (`{models: {<id>: {supported, default}}}`) — same schema as bash. **Fix**: the bash's Italian leftover `"effort: non configurabile"` becomes `"effort: not configurable"` (matches README).
- **`proxy.rs`**: see §3.

## 2. `opencc` binary (main.rs)

- clap CLI: `opencc [login] [-v|--version] [args...]` — `trailing_var_arg(true)` + **`allow_hyphen_values = true`** on the trailing `Vec<OsString>` (all flags after the first positional pass through to claude). **`disable_version_flag = true`** on the command (clap's default is `-V`; the user wants `-v`): custom `#[arg(short = 'v', long = "version", action = ArgAction::Version)]` → prints `opencc 0.1.0` from `CARGO_PKG_VERSION`. `login` is NOT a clap subcommand — manual check `args.first() == Some("login")` (faithful to bash `$1 == "login"`).
- `login` → find `codex`, spawn `codex login --device-auth`, verify `~/.codex/auth.json` has a token.
- **Migration**: `state/go/` → `state/opencode/` (move if destination missing); `last-backend` containing `go` → `opencode`; `OPENCC_BACKEND=go` still accepted, normalized to `opencode`.
- Backend menu: `1) openai`, `2) opencode`, `0) anthropic`, default = last used, enter/`d` accepts. `anthropic` → spawn `claude "$@"`, propagate exit code.
- Credentials: `OPENCODE_API_KEY` → fallback `~/.local/share/opencode/auth.json` (serde, no python). OpenAI subscription/apikey modes identical to bash (offer login when missing).
- Model + effort menus: byte-for-byte UX parity with bash (numbered list, `[ctx]`, `(last used)`, enter/d defaults, `ultra` encoded as `model@ultra`).
- Save `last-model` / `last-effort`.
- Generate picker + policy JSON.
- Proxy lifecycle: check `TcpListener::bind(127.0.0.1:$PORT)` → free (then spawn) or in-use (error); health-check `/health` for version+mode match (reject mismatched/foreign proxy — covers an old node proxy still running); spawn sibling `opencc-proxy` (`current_exe().parent()`), detached via `pre_exec(setsid)` on unix **with stdin/stdout/stderr → /dev/null / proxy.log** (setsid alone doesn't detach the tty fd), write `proxy.pid`; wait-for-health loop (25 × 100ms).
- Session registry: sweep stale, register `$PORT|$mode` before proxy start, cleanup on exit + SIGINT/SIGTERM handler (remove sess, stop proxy if last).
- Env setup for claude: `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`, `ANTHROPIC_DEFAULT_MODEL`/`ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_OPUS/SONNET/HAIKU_MODEL` (haiku = first non-reasoning model, classifier), `CLAUDE_CODE_SUBAGENT_MODEL`, `CLAUDE_CODE_MAX_CONTEXT_TOKENS`; **remove** `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_IDENTITY_TOKEN`, `ANTHROPIC_CUSTOM_HEADERS`, `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`, `CLAUDE_CODE_EFFORT_LEVEL`. All via `Command::env()/env_remove()` — required, not just preferred: the background cache-refresh thread races `std::env::set_var`.
- Launch: `claude --settings <picker> [--effort <e>] <args...>`; spawn+wait; propagate exit code (unix: `ExitStatusExt` → 128+signal).
- `-v`/`--version` handled by clap before anything else (works without claude).

## 3. `opencc-proxy` (proxy.rs) — port of opencc-proxy.mjs, 1:1, no simplifications

- Modes via `OPENCC_MODE`: `subscription` (default) | `apikey` | `opencode` (was `go`).
- Server: hyper 1 + hyper-util (`TokioIo` + `auto::Builder`, HTTP/1.1 + H2). Streaming responses via `hyper::body::Body::channel()` — `send_data` has natural backpressure; wrap the upstream read loop in `tokio::select!` so a client abort (`send_data` Err) drops the upstream body promptly. **Never set Content-Length/Transfer-Encoding** — hyper emits chunked automatically; set only `Content-Type: text/event-stream`, `Cache-Control: no-cache`, `Connection: keep-alive`.
- Routes: `GET /health` + `/healthz` → `{ok, mode, port, version: <CARGO_PKG_VERSION>, fallback}`; `GET /v1/models` + `/models` → list from `OPENCC_MODELS` CSV; `POST` with path containing `/messages` → translation paths.
- `subscription`/`apikey` path: model spec resolution (claude-* → `OPENCC_FALLBACK_MODEL`), auth from codex auth.json / `OPENAI_API_KEY`, effort policy applied, build Responses request (`input` items incl. `function_call`/`function_call_output`; `instructions`; `tools`; `reasoning.effort`), upstream `POST {base}/responses` with Bearer + `ChatGPT-Account-ID`, `store: false, stream: true` (even for non-stream clients — collect SSE → single JSON response, like node).
- **SSE upstream → Anthropic SSE**: buffer raw bytes across chunks, split on `\n`, decode complete lines; skip `: ...` comment lines and the `data: [DONE]` sentinel; parse `data: ` JSON. Port the node `blocks` Map + `nextBlockIdx` state machine **1:1** (item interleaving of text/function_call deltas is the real complexity — a naive per-event translation breaks tool use): `message_start`+`ping` once via `ensureMessageStart`, `content_block_start` (text / tool_use with `input: {}`), `content_block_delta` (`text_delta` / `input_json_delta partial_json` verbatim fragments), `content_block_stop` per output_index, `message_delta` (stop_reason tool_use|end_turn + usage payload), `message_stop`; empty-response fallback sequence.
- Usage: `response.completed`/`done` carries usage → `extract_usage` (cached_tokens → `cache_read_input_tokens`, subtract from input) + `build_usage_payload` (context_window table, `used_percentage`). No `stream_options.include_usage` (node doesn't send it; ChatGPT backend includes usage in response.completed — keep parity).
- **Turn chaining**: `Mutex<HashMap<session|agent, State>>` keyed `x-claude-code-session-id|x-claude-code-agent-id`; canonical props `{model, instructions, tools}` JSON; `is_extension` prefix match via item JSON; delta input + `previous_response_id`; **on chained-request failure retry with full input, no previous_response_id, and forgetConversation**; `forget` on props change. Port `normalize_arguments` (JSON round-trip) for the chaining baseline.
- **OAuth refresh**: on 401 (subscription only): JWT client_id (base64url via `base64` crate) → POST auth endpoint (`OPENAI_AUTH_BASE` overridable — used by tests) with refresh_token → atomic rewrite of auth.json → retry with fresh token + account_id. Refresh failure → 401 with hint.
- `opencode` mode: pass-through `POST {OPENCC_GO_BASE_URL}<path>` with `x-api-key`, `anthropic-version`/`anthropic-beta` forwarded, `Accept-Encoding: identity`; **response bytes piped raw** (no SSE parsing); headers copied minus `connection/content-length/transfer-encoding/content-encoding`. 401 if `OPENCODE_API_KEY` missing.
- `EADDRINUSE` → exit 0 silently (already-running proxy).
- Logging to stderr (→ proxy.log) with same `[opencc]` messages (effort change, delta usage, usage line).

## 4. Tests

- **Unit** (in-module): effort normalization matrix, model spec parsing, `is_extension`, `normalize_arguments`, usage extraction, TSV parse/serialize round-trip, fallback model lists, `fmt_ctx`.
- **Integration** (`tests/proxy_integration.rs`): spawn real proxy binary (`env!("CARGO_BIN_EXE_opencc-proxy")`) + mock upstream hyper server:
  - `/v1/models` listing; health payload.
  - non-stream `/v1/messages` → upstream body asserts (model, reasoning.effort, clamping with policy file), response translation.
  - `stream: true` → SSE event sequence (message_start → text deltas → message_stop), tool-use delta streaming, `[DONE]`/comment-line handling.
  - 401 → refresh retry against mock `OPENAI_AUTH_BASE` (codex auth.json with refresh_token in temp HOME).
  - opencode pass-through: header forwarding (x-api-key, anthropic-version, anthropic-beta), effort policy applied to `output_config.effort`, body relayed raw otherwise.
- **CLI-level**: menus as pure functions over injected reader/writer.

## 5. GitHub Actions

- **`ci.yml`** — on push + PR: matrix `[ubuntu-latest, windows-latest, macos-latest]`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` (tests are cross-platform: no external tools).
- **`release.yml`** — on push to master (+ `workflow_dispatch`):
  - **Build matrix** (all on `ubuntu-latest` via cargo-zigbuild; **one `macos-15` job** builds both apple targets with zigbuild using the local SDK — macos-13 runner is removed, macos-14 deprecated):
    - linux-gnu (suffix `.2.17` pins glibc 2.17 → runs on old distros incl. RPi 0/1 on Raspbian): `x86_64`, `i686`, `aarch64`, `armv7-unknown-linux-gnueabihf`, `arm-unknown-linux-gnueabihf` (armv6 — Pi 0/1), `riscv64gc`
    - linux-musl (fully static): `x86_64`, `aarch64`
    - windows-gnu: `x86_64`, `i686` (**no aarch64-pc-windows-gnu — removed from rustc/rustup**)
    - darwin: `x86_64-apple-darwin` + `aarch64-apple-darwin` (single macos-15 job)
  - Toolchain setup: `dtolnay/rust-toolchain@stable` (add targets), `ziglang/setup-zig@v2` (`version: 0.16.0`), `taiki-e/install-action@v2` (`tool: cargo-zigbuild`).
  - riscv64gc + i686-pc-windows-gnu are outside zigbuild's own CI coverage → explicit smoke step in the workflow.
  - **Release job** (needs `permissions: contents: write`; `gh` preinstalled): `VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"`; `gh release delete "v$VERSION" --yes --cleanup-tag || true` (re-runs with same version refresh the draft); `gh release create "v$VERSION" --draft --target "${{ github.sha }}"`; `gh release upload "v$VERSION" ./dist/opencc-* ./dist/opencc-proxy-* ./dist/sha256sums.txt`. Assets: `opencc-<triple>[.exe]`, `opencc-proxy-<triple>[.exe]`, `sha256sums.txt`.

## 6. install.sh + README

- `install.sh`: copy the two compiled binaries into `~/.opencc`, `chmod +x`, symlink `opencc` + `opencc-proxy` into `~/.local/bin` (drop `.mjs`), print `opencc --version`.
- `README.md`: `go` → `opencode` everywhere user-facing; prerequisites shrink (no node/curl/python3); version flag; supported platforms/architectures table; `cargo test` instructions; state layout unchanged (`~/.local/state/opencc/{opencode,openai}`); note to remove any old bash `opencc` + node proxy from PATH when upgrading (health check rejects the old proxy's version anyway).

## Verification

1. `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets` locally.
2. `cargo build --release`; run `./target/release/opencc -v` and `--version` (semver 0.1.0).
3. `OPENCC_BACKEND=anthropic ./target/release/opencc` → launches stock claude (no proxy, no menus).
4. `OPENCC_BACKEND=opencode OPENCODE_API_KEY=... ./target/release/opencc` → menus, proxy starts on 3199, health OK; verify `~/.local/state/opencc/opencode/` populated + `model-picker.json` valid JSON; exit → proxy stopped (last session).
5. Integration tests with mock upstream cover both translation paths (no real keys needed).
6. CI green on all 3 OSes; release workflow produces a draft release with all platform binaries (dry-run via workflow_dispatch on master once, then delete the draft).
