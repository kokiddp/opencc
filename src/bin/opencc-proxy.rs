//! opencc-proxy — the local Anthropic↔Responses proxy server. Spawned by the
//! `opencc` wrapper, or run standalone:
//!
//!   OPENCC_MODE=subscription opencc-proxy
//!
//! Environment variables (same as the old node proxy):
//!   OPENCC_MODE                subscription (default) | apikey | opencode
//!   OPENCC_PROXY_PORT          listening port (default: 3199)
//!   OPENAI_API_KEY             OpenAI API key (apikey mode only)
//!   OPENAI_API_BASE            API upstream (default: https://api.openai.com/v1)
//!   CHATGPT_API_BASE           subscription upstream (default: https://chatgpt.com/backend-api/codex)
//!   OPENCC_FALLBACK_MODEL      OpenAI model used when Claude Code requests claude-*
//!   OPENCC_MODELS              model list (CSV) exposed by GET /v1/models
//!   OPENCC_EFFORT_POLICY_FILE  JSON with supported/default effort per model
//!   OPENCC_GO_BASE_URL         opencode-go Anthropic upstream (opencode mode only)
//!   OPENCODE_API_KEY           upstream x-api-key (opencode mode only)
//!   OPENAI_AUTH_BASE           OAuth token endpoint (tests only)

use std::process::ExitCode;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let config = opencc::proxy::Config::from_env();
    match opencc::proxy::run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Already running — exit silently, like the node proxy.
            if err.kind() == std::io::ErrorKind::AddrInUse {
                return ExitCode::SUCCESS;
            }
            eprintln!("[opencc-proxy] error: {err}");
            ExitCode::FAILURE
        }
    }
}
