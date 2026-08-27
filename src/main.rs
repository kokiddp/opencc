//! opencc — runs Claude Code against alternative backends:
//!
//! - `opencode` — the opencode-go gateway from OpenCode
//!   (https://opencode.ai/zen/go), authenticated via `x-api-key`; a local
//!   pass-through proxy normalizes the effort for the selected model.
//! - `openai` — OpenAI models via a ChatGPT subscription (Codex CLI OAuth
//!   token) or an API key, through the local proxy `opencc-proxy`, which
//!   translates the Anthropic protocol into Responses.
//! - `anthropic` — stock Claude Code behavior: pure pass-through, without
//!   touching endpoint, authentication, model, effort or settings.
//!
//! Commands:
//!   opencc login                  ChatGPT login (Codex CLI device flow)
//!   opencc [args for claude]      backend + models + reasoning menu, then launch
//!
//! Variables:
//!   OPENCC_BACKEND    opencode | openai | anthropic  (skips the backend menu;
//!                     the legacy value `go` is still accepted)
//!   OPENCC_MODE       subscription | apikey  (openai backend only)
//!   OPENCC_PROXY_PORT local proxy port (default 3199, openai and opencode)

use clap::Parser;
use opencc::menus;
use opencc::models::{
    self, build_models_cache, fetch_openai_apikey_models, fetch_openai_subscription_models,
    fetch_opencode_models, models_cache_path, models_ids_path, openai_fallback_models,
    opencode_fallback_models, read_models_cache, Model,
};
use opencc::picker;
use opencc::state;
use opencc::util;
use std::ffi::OsString;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

const DEFAULT_PORT: u16 = 3199;

#[derive(Parser)]
#[command(
    name = "opencc",
    version,
    disable_version_flag = true,
    about = "Run Claude Code against alternative backends (OpenAI, OpenCode)"
)]
struct Cli {
    /// Print version information (opencc <semver>)
    #[arg(short = 'v', long = "version", default_value_t = false)]
    version: bool,

    /// Arguments passed through to claude
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    args: Vec<OsString>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Semver versioning: `opencc --version` / `opencc -v`.
    if cli.version {
        println!("opencc {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    // `login` is a positional (faithful to bash's `$1 == "login"`).
    if cli.args.first().and_then(|a| a.to_str()) == Some("login") {
        return if login() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    run(&cli.args)
}

// ── `opencc login`: Codex CLI device flow ──────────────────────────────────────

fn login() -> bool {
    match which::which("codex") {
        Ok(codex) => {
            println!("Starting ChatGPT login (device flow: open the link and enter the code)...");
            match Command::new(codex)
                .args(["login", "--device-auth"])
                .status()
            {
                Ok(status) if status.success() => {}
                _ => return false,
            }
        }
        Err(_) => {
            eprintln!("Error: the Codex CLI is not installed.");
            eprintln!("        Install it (https://github.com/openai/codex) or use apikey mode (OPENAI_API_KEY).");
            return false;
        }
    }
    let path = state::codex_auth_path();
    if has_codex_auth() {
        println!("Login completed: {} updated.", path.display());
        true
    } else {
        eprintln!("Error: login failed ({} has no token).", path.display());
        false
    }
}

fn has_codex_auth() -> bool {
    let Ok(text) = fs::read_to_string(state::codex_auth_path()) else {
        return false;
    };
    let Ok(auth) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    auth.pointer("/tokens/access_token")
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.is_empty())
}

fn codex_login_or_fail() -> bool {
    login()
}

// ── Main flow ──────────────────────────────────────────────────────────────────

fn run(args: &[OsString]) -> ExitCode {
    migrate_state();

    // 2) Backend selection.
    let backend = match std::env::var("OPENCC_BACKEND") {
        Ok(value) => match normalize_backend(&value) {
            Some(b) => b,
            None => {
                eprintln!("Error: invalid backend ('{value}'); use opencode, openai or anthropic.");
                return ExitCode::FAILURE;
            }
        },
        Err(_) => {
            let default = read_last_backend();
            menus::choose_backend(&mut stdin_buf(), &mut std::io::stdout(), &default)
        }
    };
    let _ = state::write_atomic_text(
        &state::state_root().join("last-backend"),
        &format!("{backend}\n"),
    );

    // The anthropic backend is a pure pass-through.
    if backend == "anthropic" {
        println!("Starting stock Claude Code (backend: anthropic).");
        return spawn_and_propagate(claude_command(args), None);
    }

    // 3) Credentials and model list.
    let state_dir = state::backend_dir(&backend);
    let key;
    let mode;
    let models: Vec<Model>;

    if backend == "opencode" {
        key = opencode_api_key();
        let Some(k) = &key else {
            eprintln!("Error: no API key found (set OPENCODE_API_KEY or log in with opencode).");
            return ExitCode::FAILURE;
        };

        let cache_path = models_cache_path(&state_dir);
        let ids_path = models_ids_path(&state_dir);
        let mut models_from_cache = read_models_cache(&cache_path);
        if models_from_cache.is_none() {
            // Build the cache on first use (never blocks startup on failure).
            if let Some(fetched) = fetch_opencode_models(k) {
                let _ = build_models_cache(&cache_path, &ids_path, &fetched);
                models_from_cache = Some(fetched);
            }
        } else if models::cache_age(&cache_path) > models::MODELS_MAX_AGE {
            // Stale cache: refresh without blocking startup.
            let key = k.clone();
            std::thread::spawn(move || {
                if let Some(fetched) = fetch_opencode_models(&key) {
                    let _ = build_models_cache(&cache_path, &ids_path, &fetched);
                }
            });
        }
        models = models_from_cache.unwrap_or_else(opencode_fallback_models);
        mode = String::new();
    } else {
        // OpenAI backend: subscription (default) or apikey.
        key = std::env::var("OPENAI_API_KEY").ok();
        let requested = std::env::var("OPENCC_MODE").ok();
        mode = match requested {
            Some(m) => m,
            None if has_codex_auth() => "subscription".to_string(),
            None if key.is_some() => "apikey".to_string(),
            None => "subscription".to_string(),
        };
        if mode != "subscription" && mode != "apikey" {
            eprintln!("Error: invalid OPENCC_MODE ('{mode}'); use subscription or apikey.");
            return ExitCode::FAILURE;
        }
        if mode == "apikey" && key.is_none() {
            eprintln!("Error: apikey mode requires OPENAI_API_KEY.");
            return ExitCode::FAILURE;
        }

        // Missing subscription auth: offer the login.
        if mode == "subscription" && !has_codex_auth() {
            eprintln!(
                "ChatGPT authentication not found ({} missing or without a token).",
                state::codex_auth_path().display()
            );
            if menus::ask_yes_no(
                "Log in now (device flow via Codex)? [y/N] ",
                &mut stdin_buf(),
                &mut std::io::stdout(),
            ) && !codex_login_or_fail()
            {
                return ExitCode::FAILURE;
            }
            if !has_codex_auth() {
                eprintln!(
                    "Error: no login completed. Retry with `opencc login` or set OPENAI_API_KEY."
                );
                return ExitCode::FAILURE;
            }
        }

        models = if mode == "subscription" {
            fetch_openai_subscription_models(&state::codex_models_cache_path())
                .unwrap_or_else(openai_fallback_models)
        } else if let Some(k) = &key {
            fetch_openai_apikey_models(k).unwrap_or_else(openai_fallback_models)
        } else {
            openai_fallback_models()
        };
    }

    if models.is_empty() {
        eprintln!("Error: no models available.");
        return ExitCode::FAILURE;
    }

    // 4) Default = last used (if still available).
    let fallback_default = if backend == "opencode" {
        if models
            .iter()
            .any(|m| m.slug == models::OPENCODE_FALLBACK_MODEL)
        {
            models::OPENCODE_FALLBACK_MODEL.to_string()
        } else {
            models[0].slug.clone()
        }
    } else {
        models[0].slug.clone()
    };
    let mut default = fallback_default.clone();
    let mut last_used = String::new();
    let state_file = state_dir.join("last-model");
    if let Ok(stored) = fs::read_to_string(&state_file) {
        let stored = stored.trim();
        if models.iter().any(|m| m.slug == stored) {
            default = stored.to_string();
            last_used = stored.to_string();
        }
    }

    // 5) Model menu.
    let header = if backend == "opencode" {
        "OpenCode models:"
    } else {
        &format!("OpenAI models (auth: {mode}):")
    };
    menus::print_model_list(&models, &last_used, header, &mut std::io::stdout());
    let model = menus::choose_model(&mut stdin_buf(), &mut std::io::stdout(), &models, &default);

    // 6) Reasoning level (effort). On the opencode backend, `toggle` models
    // (always-on reasoning) expose no levels: the prompt is skipped.
    let valid_efforts: Vec<String> = if backend == "opencode" {
        models::efforts_csv(&model_efforts_of(&models, &model))
    } else {
        let csv = model_efforts_of(&models, &model);
        if csv.is_empty() {
            "low,medium,high".to_string()
        } else {
            csv
        }
        .split(',')
        .map(String::from)
        .collect()
    };
    let effort_saved = fs::read_to_string(state_dir.join("last-effort"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let model_default = model_default_of(&models, &model);
    let effort = if valid_efforts.is_empty() {
        String::new()
    } else {
        menus::choose_effort(
            &mut stdin_buf(),
            &mut std::io::stdout(),
            &valid_efforts,
            &effort_saved,
            &model_default,
        )
    };

    // Claude Code sends efforts low..max in output_config.effort. `ultra` is
    // not available in /effort, so it stays encoded in the model as @ultra.
    let model_effort = if effort == "ultra" {
        format!("{model}@ultra")
    } else {
        model.clone()
    };

    // 7) Save the last model + effort.
    let _ = state::write_atomic_text(&state_file, &format!("{model}\n"));
    let _ = state::write_atomic_text(&state_dir.join("last-effort"), &format!("{effort}\n"));

    // 8) Generate the model picker + effort policy for Claude Code.
    let picker_description = if backend == "opencode" {
        "OpenCode via opencc".to_string()
    } else {
        format!("OpenAI via opencc ({mode})")
    };
    let (picker_settings, effort_policy) =
        match picker::write_picker_files(&state_dir, &models, &picker_description) {
            Ok(paths) => paths,
            Err(err) => {
                eprintln!("Error: cannot write the settings files: {err}");
                return ExitCode::FAILURE;
            }
        };

    // 9) Local proxy: start it, or reuse it if already consistent.
    let proxy_mode = if backend == "opencode" {
        "opencode".to_string()
    } else {
        mode.clone()
    };
    let port = std::env::var("OPENCC_PROXY_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);

    // Session registry: the proxy closes with the last session. Registered
    // before ensure_proxy so another session exiting while this one is still
    // starting up does not yank the proxy away.
    let my_pid = std::process::id();
    state::sweep_stale_sessions(&backend);
    if let Err(err) = state::register_session(&backend, my_pid, port, &proxy_mode) {
        eprintln!("Error: cannot register the session: {err}");
        return ExitCode::FAILURE;
    }
    let cleanup = Cleanup {
        backend: backend.clone(),
        port,
        proxy_mode: proxy_mode.clone(),
    };
    cleanup.install_signal_handler();

    if let Err(msg) = ensure_proxy(ProxyParams {
        backend: &backend,
        state_dir: &state_dir,
        openai_key: &key,
        proxy_mode: &proxy_mode,
        port,
        model_effort: &model_effort,
        models: &models,
        effort_policy: &effort_policy,
    }) {
        eprintln!("Error: {msg}");
        cleanup.run();
        return ExitCode::FAILURE;
    }

    // 10) Exports and launch.
    let mut cmd = claude_command(args);
    cmd.arg("--settings").arg(&picker_settings);
    cmd.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"));
    if backend == "openai" && mode == "apikey" {
        cmd.env("ANTHROPIC_API_KEY", key.unwrap_or_default());
    } else {
        // The proxy handles upstream auth; any non-empty local value is
        // enough to disable Claude Code's OAuth login.
        cmd.env("ANTHROPIC_API_KEY", format!("opencc-{proxy_mode}"));
    }
    cmd.env("ANTHROPIC_DEFAULT_MODEL", &model_effort)
        .env("ANTHROPIC_MODEL", &model_effort)
        .env("ANTHROPIC_DEFAULT_OPUS_MODEL", &model_effort)
        .env("ANTHROPIC_DEFAULT_SONNET_MODEL", &model_effort)
        .env("CLAUDE_CODE_SUBAGENT_MODEL", &model_effort);
    // The haiku alias feeds the auto-mode classifier and the background
    // tasks. The classifier uses max_tokens=1: reasoning models spend that
    // token on thinking and produce no text, so auto mode fails. We pin the
    // alias to a small model WITHOUT reasoning.
    let classifier = if backend == "opencode" {
        models
            .iter()
            .find(|m| model_efforts_of(&models, &m.slug).is_empty())
            .map(|m| m.slug.clone())
            .unwrap_or_else(|| models::OPENCODE_FALLBACK_MODEL.to_string())
    } else {
        let mini = models
            .iter()
            .find(|m| m.slug.contains("mini"))
            .map(|m| m.slug.clone());
        match mini {
            Some(m) if models.iter().any(|x| x.slug == m) => m,
            _ => models[0].slug.clone(),
        }
    };
    cmd.env("ANTHROPIC_DEFAULT_HAIKU_MODEL", classifier);
    if let Some(ctx) = model_context_of(&models, &model) {
        cmd.env("CLAUDE_CODE_MAX_CONTEXT_TOKENS", ctx.to_string());
    }
    // The effort must be passed with --effort, which sets the level of the
    // session: it stays changeable with /effort. The
    // CLAUDE_CODE_EFFORT_LEVEL variable would instead pin it for the whole
    // process, making /effort ineffective.
    if !effort.is_empty()
        && effort != "ultra"
        && !args
            .iter()
            .any(|a| a.to_string_lossy().contains("--effort"))
    {
        cmd.arg("--effort").arg(&effort);
    }
    // Unset whatever the environment has: these must come from us.
    for var in [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_IDENTITY_TOKEN",
        "ANTHROPIC_CUSTOM_HEADERS",
        "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
        "CLAUDE_CODE_EFFORT_LEVEL",
    ] {
        cmd.env_remove(var);
    }

    let ctxstr = model_context_of(&models, &model)
        .map(|c| format!(", context: {}", util::fmt_ctx(c)))
        .unwrap_or_default();
    let effstr = if effort.is_empty() {
        String::new()
    } else {
        format!(", reasoning: {effort}")
    };
    if backend == "opencode" {
        println!("Starting Claude Code with model: {model} (backend: opencode{ctxstr}{effstr})");
    } else {
        println!(
            "Starting Claude Code with model: {model} (backend: openai/{mode}{ctxstr}{effstr})"
        );
    }
    println!("During the session: /model changes the model, /effort changes the reasoning.");

    spawn_and_propagate(cmd, Some(&cleanup))
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn stdin_buf() -> BufReader<std::io::Stdin> {
    BufReader::new(std::io::stdin())
}

/// Normalizes the backend id: the legacy `go` value becomes `opencode`.
fn normalize_backend(value: &str) -> Option<String> {
    match value {
        "go" | "opencode" => Some("opencode".to_string()),
        "openai" | "anthropic" => Some(value.to_string()),
        _ => None,
    }
}

fn read_last_backend() -> String {
    fs::read_to_string(state::state_root().join("last-backend"))
        .ok()
        .map(|s| s.trim().to_string())
        .and_then(|b| normalize_backend(&b))
        .unwrap_or_else(|| "openai".to_string())
}

/// Migrates state from the old bash versions: the `go/` directory becomes
/// `opencode/`, a stored `go` backend becomes `opencode`, and the pre-v2
/// state directly in the state root moves under `openai/`.
fn migrate_state() {
    let root = state::state_root();
    let go = root.join("go");
    let opencode = root.join("opencode");
    if go.is_dir() && !opencode.exists() {
        let _ = fs::rename(&go, &opencode);
    }
    let backend_file = root.join("last-backend");
    if let Ok(content) = fs::read_to_string(&backend_file) {
        if content.trim() == "go" {
            let _ = state::write_atomic_text(&backend_file, "opencode\n");
        }
    }
    for f in ["last-model", "last-effort"] {
        let src = root.join(f);
        let dst = root.join("openai").join(f);
        if src.exists() && !dst.exists() {
            let _ = fs::rename(&src, &dst);
        }
    }
    let _ = fs::create_dir_all(root.join("opencode"));
    let _ = fs::create_dir_all(root.join("openai"));
}

fn opencode_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("OPENCODE_API_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }
    let text = fs::read_to_string(state::opencode_auth_path()).ok()?;
    let Ok(auth) = serde_json::from_str::<serde_json::Value>(&text) else {
        return None;
    };
    auth.pointer("/opencode-go/key")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .map(String::from)
}

fn model_efforts_of(models: &[Model], slug: &str) -> String {
    models
        .iter()
        .find(|m| m.slug == slug)
        .map(|m| m.efforts.clone())
        .unwrap_or_default()
}

fn model_default_of(models: &[Model], slug: &str) -> String {
    models
        .iter()
        .find(|m| m.slug == slug)
        .map(|m| m.default.clone())
        .unwrap_or_default()
}

fn model_context_of(models: &[Model], slug: &str) -> Option<u64> {
    models
        .iter()
        .find(|m| m.slug == slug)
        .and_then(|m| (m.context > 0).then_some(m.context))
}

/// Builds the `claude` command with the passthrough args.
fn claude_command(args: &[OsString]) -> Command {
    let mut cmd = Command::new("claude");
    cmd.args(args);
    cmd
}

/// Spawns a command, waits for it and propagates its exit code (unix:
/// 128+signal). `cleanup` runs before exiting (the proxy session registry).
fn spawn_and_propagate(mut cmd: Command, cleanup: Option<&Cleanup>) -> ExitCode {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Keep claude in our process group (so Ctrl+C reaches it too) but
        // restore the default handlers: opencc intercepts them to run the
        // cleanup before exiting.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                Ok(())
            });
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("Error: 'claude' not found on PATH (Claude Code is required by opencc).");
            if let Some(cleanup) = cleanup {
                cleanup.run();
            }
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("Error: cannot launch claude: {err}");
            if let Some(cleanup) = cleanup {
                cleanup.run();
            }
            return ExitCode::FAILURE;
        }
    };
    let status = child.wait();
    let interrupted = crate::signals::interrupted_signal();
    if let Some(cleanup) = cleanup {
        cleanup.run();
    }
    match status {
        Ok(status) => {
            if let Some(sig) = interrupted {
                return exit_code_from_signal(sig);
            }
            exit_code_from_status(status)
        }
        Err(_) => ExitCode::FAILURE,
    }
}

fn exit_code_from_status(status: ExitStatus) -> ExitCode {
    if let Some(code) = status.code() {
        return ExitCode::from(code.min(255) as u8);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return exit_code_from_signal(sig);
        }
    }
    ExitCode::FAILURE
}

fn exit_code_from_signal(sig: i32) -> ExitCode {
    ExitCode::from((128 + sig).min(255) as u8)
}

// ── Proxy lifecycle ────────────────────────────────────────────────────────────

fn proxy_bin_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(windows) {
        "opencc-proxy.exe"
    } else {
        "opencc-proxy"
    };
    let candidate = dir.join(name);
    if candidate.exists() {
        Some(candidate)
    } else {
        // Fall back to PATH (useful when the wrapper was invoked through a
        // symlink into a directory without the proxy next to it).
        which::which(name).ok()
    }
}

fn proxy_health(port: u16) -> Option<serde_json::Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()?;
    client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .ok()
        .and_then(|r| r.json().ok())
}

/// Everything ensure_proxy needs about the current session.
struct ProxyParams<'a> {
    backend: &'a str,
    state_dir: &'a Path,
    openai_key: &'a Option<String>,
    proxy_mode: &'a str,
    port: u16,
    model_effort: &'a str,
    models: &'a [Model],
    effort_policy: &'a Path,
}

/// Starts the local proxy (or reuses a consistent one already running).
/// Returns Err with a user-facing message on failure.
fn ensure_proxy(p: ProxyParams) -> Result<(), String> {
    let ProxyParams {
        backend,
        state_dir,
        openai_key,
        proxy_mode,
        port,
        model_effort,
        models,
        effort_policy,
    } = p;
    // Already running and consistent? Reuse it.
    if let Some(health) = proxy_health(port) {
        let version = health.get("version").and_then(|v| v.as_str()).unwrap_or("");
        let mode = health.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        if version == opencc::proxy::PROXY_VERSION && mode == proxy_mode {
            return Ok(());
        }
        return Err(format!(
            "an incompatible opencc proxy is already listening on port {port} \
             (different version or mode).\n        Close it manually or use a different port with OPENCC_PROXY_PORT."
        ));
    }

    // Port in use by a foreign process?
    if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
        return Err(format!(
            "port {port} is in use by another process.\n        Set OPENCC_PROXY_PORT to a free port."
        ));
    }

    let proxy_bin = proxy_bin_path().ok_or_else(|| {
        format!(
            "opencc-proxy not found (missing next to {})",
            std::env::current_exe()
                .map(|e| e.display().to_string())
                .unwrap_or_default()
        )
    })?;

    let models_csv = models
        .iter()
        .map(|m| m.slug.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let log_path = state_dir.join("proxy.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| format!("cannot open {}: {err}", log_path.display()))?;

    let mut cmd = Command::new(&proxy_bin);
    cmd.env("OPENCC_MODE", proxy_mode)
        .env("OPENCC_PROXY_PORT", port.to_string())
        .env("OPENAI_API_KEY", openai_key.clone().unwrap_or_default())
        .env(
            "OPENCODE_API_KEY",
            std::env::var("OPENCODE_API_KEY").unwrap_or_default(),
        )
        .env("OPENCC_GO_BASE_URL", models::OPENCODE_BASE_URL)
        .env("OPENCC_FALLBACK_MODEL", model_effort)
        .env("OPENCC_MODELS", &models_csv)
        .env("OPENCC_EFFORT_POLICY_FILE", effort_policy)
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .map_err(|err| format!("cannot clone proxy.log: {err}"))?,
        )
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // Detach from the controlling terminal (like nohup): the
                // proxy must survive the wrapper and stay up until the last
                // session closes it.
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            return Err(format!(
                "cannot start the proxy: {err} (log: {})",
                log_path.display()
            ))
        }
    };
    let pid = child.id();
    // Detached: the proxy outlives the wrapper (dropping the Child does not
    // kill it, but we never wait on it either).
    std::mem::forget(child);
    let _ = state::write_atomic_text(&state_dir.join("proxy.pid"), &format!("{pid}\n"));

    // Wait for health (25 × 100ms, like the bash script).
    for _ in 0..25 {
        if proxy_health(port).is_some() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = backend;
    Err(format!(
        "the proxy failed to start (log: {})",
        log_path.display()
    ))
}

/// Per-session cleanup: remove the session file, sweep the registry, and stop
/// the proxy when this was the last session on it.
struct Cleanup {
    backend: String,
    port: u16,
    proxy_mode: String,
}

impl Cleanup {
    fn run(&self) {
        let my_pid = std::process::id();
        state::unregister_session(&self.backend, my_pid);
        state::sweep_stale_sessions(&self.backend);
        if state::sessions_on_proxy(&self.backend, self.port, &self.proxy_mode) == 0 {
            let health = proxy_health(self.port);
            let consistent = health.as_ref().is_some_and(|h| {
                h.get("version").and_then(|v| v.as_str()) == Some(opencc::proxy::PROXY_VERSION)
                    && h.get("mode").and_then(|v| v.as_str()) == Some(&self.proxy_mode)
            });
            if consistent {
                let pid_file = state::backend_dir(&self.backend).join("proxy.pid");
                if let Ok(text) = fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        let _ = state::kill_process(pid);
                    }
                }
                let _ = fs::remove_file(&pid_file);
                eprintln!("opencc proxy stopped (last session closed).");
            }
        }
    }

    #[cfg(unix)]
    fn install_signal_handler(&self) {
        // The handler only records the signal; the cleanup runs after claude
        // exits (claude receives the same signal from the terminal).
        crate::signals::install();
    }

    #[cfg(not(unix))]
    fn install_signal_handler(&self) {}
}

#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicI32, Ordering};

    static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

    extern "C" fn handler(sig: libc::c_int) {
        INTERRUPTED.store(sig, Ordering::SeqCst);
    }

    pub fn install() {
        unsafe {
            libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        }
    }

    pub fn interrupted_signal() -> Option<i32> {
        let sig = INTERRUPTED.load(Ordering::SeqCst);
        if sig == 0 {
            None
        } else {
            Some(sig)
        }
    }
}
