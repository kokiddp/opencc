# opencc

Rust wrapper for running **Claude Code** against alternative backends, with a
native local proxy (no Node, no Python, no curl):

| Backend    | What it uses | Authentication | Proxy |
|------------|--------------|----------------|-------|
| `openai`   | OpenAI models (GPT-5.x) | ChatGPT subscription (Codex OAuth) or `OPENAI_API_KEY` | yes (Anthropic→Responses translation) |
| `opencode` | the [opencode-go](https://opencode.ai/zen/go) gateway from OpenCode | `x-api-key` header | yes (Anthropic pass-through) |
| `anthropic`| stock Claude Code | unchanged (native behavior) | no |

On the `openai` and `opencode` backends: numbered model menu with context
size, **reasoning level** selection, memory of the last choice (per backend)
and automatic configuration of Claude Code's environment variables. The
`anthropic` backend is a pure pass-through: it launches `claude` without
touching endpoint, authentication, model, effort or settings.

Everything is compiled: the wrapper and the proxy are two static-friendly
Rust binaries with **no runtime dependencies** beyond `claude` itself
(`codex` is only needed for `opencc login`, `opencode` for the opencode CLI
login).

## Installation

Installers for every supported platform:

- **Linux / macOS:** `./install.sh`
- **Windows:** `.\install.ps1` (from PowerShell, or
  `powershell -ExecutionPolicy Bypass -File .\install.ps1`)

Both installers detect the OS and architecture, and pick the binaries from
the first available source, in this order:

1. a **freshly built** binary for this exact platform
   (`target/<triple>/release/`, then `target/release/`) — local builds are
   only used if they actually run on this machine;
2. a prebuilt binary copied **next to the installer**;
3. the **latest GitHub release** for the detected platform, downloaded and
   verified against `sha256sums.txt`.

For example, on an ARMv6 Raspberry Pi 0/1 the installer downloads
`arm-unknown-linux-gnueabihf`; on a musl system (Alpine) it prefers the fully
static `x86_64-unknown-linux-musl` build, which runs anywhere.

The scripts check that **Claude Code** (`claude`) is installed — if it is
missing, they offer to install it (official installer via `curl`, `npm` as a
fallback) — and only then install `opencc` and `opencc-proxy` into
`~/.opencc/`, linking them into `~/.local/bin/` (copied, on Windows —
symlinks need elevation):

```bash
~/.local/bin/opencc        -> ~/.opencc/opencc
~/.local/bin/opencc-proxy  -> ~/.opencc/opencc-proxy
```

Re-running an installer after an update refreshes the files in `~/.opencc`
without touching the links. The two binaries must stay side by side: `opencc`
looks for `opencc-proxy` in its own directory (required by the `openai` and
`opencode` backends).

Environment overrides (both installers): `OPENCC_REPO` (GitHub repo, for
forks/mirrors), `OPENCC_INSTALL_DIR`, `OPENCC_BIN_DIR`,
`OPENCC_DOWNLOAD_BASE` (mirrors/tests).

> **Upgrading from the old bash/Node version:** remove the old `opencc` shell
> script and `opencc-proxy.mjs` from your PATH (`~/.opencc` is a good place
> to check). The new wrapper refuses to reuse a running proxy of a different
> version, so an old node proxy still listening on the default port must be
> closed manually (or with a different `OPENCC_PROXY_PORT`). Your existing
> state is migrated automatically: `go/` becomes `opencode/`, and the stored
> backend, models and effort are kept.

## Prerequisites

- `claude` installed (Claude Code)
- `openai` backend:
  - **subscription (default):** the [Codex CLI](https://github.com/openai/codex)
    for login. The OAuth token lives in `~/.codex/auth.json` (written by
    `opencc login`); the model list is read from `~/.codex/models_cache.json`.
  - **apikey:** a key in `OPENAI_API_KEY`.
- `opencode` backend: an API key in `OPENCODE_API_KEY` or the file
  `~/.local/share/opencode/auth.json` (login with `opencode`).

## Usage

```bash
opencc --version               # semver (also: opencc -v)
opencc login                   # generates/refreshes ~/.codex/auth.json (device flow)
opencc [args for claude]       # backend + model + reasoning menu, then launches
```

At startup `opencc` asks for:

1. the **backend**:
   - `1` `openai` — OpenAI models (local proxy);
   - `2` `opencode` — OpenCode gateway;
   - `0` `anthropic` — stock Claude Code (pass-through, no changes).

   The default is the last one used; the menu can be skipped by setting
   `OPENCC_BACKEND=openai|opencode|anthropic` (the legacy value `go` is still
   accepted and normalized to `opencode`).
2. the **model**, with context size and a marker for the last one used;
3. the **reasoning level** valid for that model.

Press enter (or `d`) to accept the defaults. On the `opencode` backend,
models with always-on reasoning (e.g. `minimax-m3`) skip step 3.

## Changing model and reasoning mid-session

The choices made at launch are only the session **defaults**: `/model` and
`/effort` remain active and apply to subsequent requests without a restart.
`opencc` generates a `model-picker.json` for the current backend and loads it
via `--settings`, so `/model` shows the OpenAI or OpenCode models instead of
the stock Anthropic aliases. Every entry lists, in its description, the
**efforts the model actually supports** and its default (e.g.
`OpenAI via opencc · effort: low, medium, high, xhigh, max (default: medium)`);
models without configurable reasoning are marked `effort: not configurable`.

Claude Code's automatic model discovery is not used: even if the backend
exposes a valid `GET /v1/models`, Claude Code drops by design every ID that
does not contain `claude` or `anthropic` (so `gpt-*`, `minimax-*`, etc.). The
native `modelPicker` accepts arbitrary IDs instead and forwards them without
renaming.

### Limitation: the `/effort` picker is global

Claude Code has no knowledge of custom-model capabilities and does **not**
allow filtering `/effort` levels per selected model: `modelPicker` entries only
accept an ID, a label and a description, and there is no way to declare
supported efforts or a default for arbitrary IDs. The `/effort` picker stays
global.

The fix happens in the proxy, which receives every request with the chosen
model and effort and applies the model's **real policy** (from
`model-efforts.json`):

- unsupported level → **clamped down** to the highest available level not
  exceeding the requested one (e.g. `xhigh` → `high` on a model without
  `xhigh`); if none is lower, to the lowest available one;
- unknown level → the model's default;
- model without effort → the effort is **removed**;
- no effort chosen → the model's **default**.

When the proxy changes the effort, it logs a line in its own log
(`~/.local/state/opencc/<backend>/proxy.log`), e.g.:
`[opencc] effort gpt-two: max -> high (clamped)`.

Other known limitations:

- **`ultra`** (GPT-5.6 only) is not a level `/effort` accepts: it can only be
  chosen from the initial menu, where it is encoded as `model@ultra`.
- `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is set from the model chosen at launch and
  does not follow `/model` changes.

> **Note:** the effort is passed with the `--effort` flag and **not** with the
> `CLAUDE_CODE_EFFORT_LEVEL` variable: that variable pins the level for the
> whole process and makes `/effort` ineffective (Claude Code would keep
> sending the env value). If present in the environment, `opencc` removes it.

### Environment variables

| Variable | Effect |
|----------|--------|
| `OPENCC_BACKEND` | `openai` \| `opencode` \| `anthropic` — skips the backend menu (`go` accepted for compatibility) |
| `OPENCC_MODE` | `subscription` \| `apikey` — forces OpenAI authentication |
| `OPENCC_PROXY_PORT` | local proxy port (default `3199`, `openai`/`opencode` backends) |
| `OPENAI_API_KEY` | OpenAI key (`apikey` mode) |
| `OPENCODE_API_KEY` | OpenCode gateway key |

## `openai` backend

Claude Code speaks **only** the Anthropic protocol (`/v1/messages`), while the
OpenAI backends speak the Responses protocol: `opencc-proxy` is a local proxy
(bound to `127.0.0.1` only) that translates the requests and forwards them to

- **subscription** → `https://chatgpt.com/backend-api/codex/responses`, with
  the OAuth token from `~/.codex/auth.json` (uses your ChatGPT
  Plus/Pro/Team plan);
- **apikey** → `https://api.openai.com/v1/responses`, with `OPENAI_API_KEY`.

- **Login:** `opencc login` starts the Codex CLI device flow. If
  authentication is missing, `opencc` offers it at startup.
- **Automatic refresh:** tokens last ~24h; on 401 the proxy refreshes them via
  `refresh_token` and rewrites `~/.codex/auth.json`. If the refresh fails,
  just run `opencc login` again.
- **Models:** from `~/.codex/models_cache.json` (subscription) or from
  OpenAI's `/v1/models` (apikey); falls back to a static list.
- **Context:** `max_context_window × effective_context_window_percent` (e.g.
  ~828K for GPT-5.6), exported as `CLAUDE_CODE_MAX_CONTEXT_TOKENS`.
- **Reasoning:** Claude Code sends `output_config: { effort }` and the proxy
  translates it into `reasoning: { effort }`, normalizing it against the
  model's policy (see above). `ultra` is not accepted by `--effort`/`/effort`
  (it would be silently ignored), so it stays encoded in the model as
  `model@ultra`, a format the proxy recognizes.
- **Turn chaining (input savings):** the proxy never resends the full history
  like codex does: if the new request of the same session is an extension of
  the previous one, it sends **only the delta** with `previous_response_id`;
  the backend reconnects the context and bills the repeated part at cache
  rates. If chaining fails (expired response, changed context), the proxy
  automatically retries with the full request. The log records every request:
  `[opencc] delta sess-1|: 1 items sent (baseline 2)` and
  `[opencc] usage gpt-5.6-sol: in=... cached=... out=... (delta|full)`.
- **`/usage`:** the tokens (input/output) shown by `/usage` are the real ones
  from the OpenAI backend: the proxy converts
  `input_tokens_details.cached_tokens` into `cache_read_input_tokens` and
  subtracts it from `input_tokens`, per Anthropic convention. Two limits: the
  **cache read/write** columns show 0 (the Responses API only reports usage at
  the end of the stream, while Claude Code reads the cache from
  `message_start`), and the **cost** is a Claude Code estimate for unknown
  models, marked `costs may be inaccurate due to usage of unknown models` —
  it is not possible to inject provider pricing.

## `opencode` backend

- **Models:** list from the gateway's `/v1/models`; **context and effort**
  from the upstream catalog `https://models.opencode.ai/api.json` (provider
  `opencode-go`). Cached in `~/.local/state/opencc/opencode/models.tsv`,
  refreshed in the background after 7 days; without a cache the `minimax-m3`
  fallback is used.
- **Reasoning:** the model's valid levels are intersected with those accepted
  by Claude Code (`low,medium,high,xhigh,max`).
- **`/usage`:** being an Anthropic pass-through, the gateway's usage reaches
  Claude Code without conversions: tokens and cache are what opencode-go
  reports (if it includes them); the cost remains an estimate for unknown
  models.
- **Proxy pass-through:** `opencc` routes the `opencode` backend through the
  same local proxy, in **Anthropic pass-through** mode: the proxy only
  modifies `model` and `output_config.effort` (applying the policy) and
  forwards the rest of the request and response untranslated. The proxy asks
  the upstream for `Accept-Encoding: identity` and does not forward
  `Content-Encoding`: this prevents the already-decompressed body from being
  decompressed again by Claude Code (BrotliDecompressionError). The proxy
  authenticates with `OPENCODE_API_KEY` (or the value from `auth.json`) in
  `x-api-key`.

## Auto mode (classifier)

Claude Code's auto mode uses the safety classifier through the **haiku**
alias with `max_tokens: 1`. Reasoning models spend that single token on
`thinking` and produce no text: the classifier fails and auto mode reports
*"auto mode cannot determine the safety"*. That is why `opencc` pins
`ANTHROPIC_DEFAULT_HAIKU_MODEL` to a dedicated model **without reasoning**:

- `opencode` backend → the first catalog model with no effort levels (default
  `minimax-m3`);
- `openai` backend → the first `*mini*` model (default `gpt-5.4-mini`).

The main model (opus/sonnet, subagents) is unchanged. The `anthropic` backend
uses native behavior.

## Automatic proxy shutdown

When you exit Claude Code, `opencc` stops the proxy **if no other sessions
remain active** on the same proxy (port+mode). Every invocation registers a
file in `~/.local/state/opencc/<backend>/sessions/<pid>.sess`; on exit the
file is removed and, if it was the last one, the proxy is terminated. Session
files left behind by abnormally terminated sessions (dead PID) are swept at
the next startup. The `anthropic` backend does not use the proxy: no
registration.

## `anthropic` backend

Selecting `anthropic` makes `opencc` run `claude` with no changes at all: no
proxy, no gateway environment variables, no model menu and no generated
`--settings`. Behavior is stock Claude Code, with whatever configuration your
environment has. It is the only backend with zero setup beyond `claude`.

## Local state

`~/.local/state/opencc/` (on Windows: `%LOCALAPPDATA%\opencc\`):

```
last-backend         last backend used
openai/              last-model, last-effort, model-picker.json,
                     model-efforts.json, proxy.log, proxy.pid
opencode/            last-model, last-effort, model-picker.json,
                     model-efforts.json, models.tsv, models.ids
```

## Supported platforms

The release workflow builds and attaches binaries for every target below
(`opencc-<triple>` + `opencc-proxy-<triple>`, `.exe` on Windows, plus
`sha256sums.txt`):

| Family | Architectures |
|--------|---------------|
| Linux (glibc ≥ 2.17) | x86_64, i686, aarch64, armv7 (gnueabihf), armv6 (gnueabihf — Raspberry Pi 0/1), riscv64 |
| Linux (musl, fully static) | x86_64, aarch64 |
| Windows | x86_64, i686 |
| macOS | x86_64 (Intel), aarch64 (Apple Silicon) |

In practice it supports any platform that can run Claude Code. To build from
source on a platform not listed, `cargo build --release` works anywhere the
Rust toolchain runs.

## Development

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets
```

The test suite covers the translation logic (effort policy, turn chaining,
usage conversion) with unit tests and the full proxy end-to-end: the real
`opencc-proxy` binary is spawned against a mock upstream, so no API keys are
needed. CI (GitHub Actions) runs the same checks on Linux, Windows and macOS,
and pushes to `master` draft a semver release with all the binaries above.

## Notes

- **Free plan:** the subscription backend requires a paid ChatGPT plan
  (Plus/Pro/Team).
- **Draft releases are not downloadable:** the installers fetch from
  `releases/latest`, which GitHub resolves to the latest *published* release.
  Until a draft is published, the installer says so and falls back to building
  from source.
- The translation logic in the proxy is adapted from the MIT-licensed proxy of
  [codex-for-claude-code](https://github.com/Yusang-park/codex-for-claude-code).
