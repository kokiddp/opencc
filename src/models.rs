//! Model discovery and caching.
//!
//! The opencode and openai backends expose their models through the same TSV
//! format the bash script used: `slug<TAB>display<TAB>context<TAB>efforts<TAB>default`.

use crate::effort::CLAUDE_EFFORTS;
use crate::util::now_unix;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const OPENCODE_BASE_URL: &str = "https://opencode.ai/zen/go";
pub const OPENCODE_MODELS_URL: &str = "https://opencode.ai/zen/go/v1/models";
pub const OPENCODE_CATALOG_URL: &str = "https://models.opencode.ai/api.json";
pub const OPENCODE_FALLBACK_MODEL: &str = "minimax-m3";
/// Model cache freshness: refresh after 7 days.
pub const MODELS_MAX_AGE: u64 = 7 * 24 * 3600;

/// Fallback OpenAI models when the list cannot be fetched, with their effort
/// policies (from the bash script).
pub const OPENAI_FALLBACK_MODELS: &[(&str, &[&str], &str)] = &[
    (
        "gpt-5.6-sol",
        &["low", "medium", "high", "xhigh", "max", "ultra"],
        "low",
    ),
    (
        "gpt-5.6-terra",
        &["low", "medium", "high", "xhigh", "max", "ultra"],
        "low",
    ),
    (
        "gpt-5.6-luna",
        &["low", "medium", "high", "xhigh", "max", "ultra"],
        "low",
    ),
    ("gpt-5.5", &["low", "medium", "high", "xhigh"], "medium"),
    ("gpt-5.4", &["low", "medium", "high", "xhigh"], "medium"),
    (
        "gpt-5.4-mini",
        &["low", "medium", "high", "xhigh"],
        "medium",
    ),
];

/// A model entry. `context` is 0 when unknown (the TSV column stays empty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub slug: String,
    pub display: String,
    pub context: u64,
    /// Reasoning levels the model accepts, as CSV ("low,medium,high").
    pub efforts: String,
    pub default: String,
}

impl Model {
    /// Parses one TSV line (5 tab-separated fields; empty fields allowed).
    fn from_tsv_line(line: &str) -> Option<Model> {
        let fields: Vec<&str> = line.split('\t').collect();
        let slug = fields.first().copied().unwrap_or("").trim();
        if slug.is_empty() {
            return None;
        }
        let display = fields.get(1).copied().unwrap_or("").to_string();
        let context = fields
            .get(2)
            .copied()
            .unwrap_or("")
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        let efforts = fields.get(3).copied().unwrap_or("").to_string();
        let default = fields.get(4).copied().unwrap_or("").to_string();
        Some(Model {
            slug: slug.to_string(),
            display: if display.is_empty() {
                slug.to_string()
            } else {
                display.to_string()
            },
            context,
            efforts,
            default,
        })
    }

    fn to_tsv_line(&self) -> String {
        let ctx = if self.context > 0 {
            self.context.to_string()
        } else {
            String::new()
        };
        format!(
            "{}\t{}\t{}\t{}\t{}",
            self.slug, self.display, ctx, self.efforts, self.default
        )
    }
}

/// Parses a TSV document (empty lines skipped).
pub fn parse_tsv(text: &str) -> Vec<Model> {
    text.lines().filter_map(Model::from_tsv_line).collect()
}

/// Serializes models to the TSV format.
pub fn serialize_tsv(models: &[Model]) -> String {
    let mut out = String::new();
    for m in models {
        out.push_str(&m.to_tsv_line());
        out.push('\n');
    }
    out
}

/// Parses the effort CSV into a list (empty → no levels).
pub fn efforts_csv(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

// ── Fetchers ───────────────────────────────────────────────────────────────────

fn json_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .build()
        .expect("reqwest client builds")
}

/// opencode backend: model IDs from the gateway's `/v1/models` (x-api-key),
/// context and effort from the upstream catalog. Returns None if the gateway
/// list cannot be fetched (the caller falls back to the TSV cache / fallback
/// model).
pub fn fetch_opencode_models(key: &str) -> Option<Vec<Model>> {
    let client = json_client();
    let resp = client
        .get(OPENCODE_MODELS_URL)
        .timeout(std::time::Duration::from_secs(15))
        .header("x-api-key", key)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let list: serde_json::Value = resp.json().ok()?;
    let ids: Vec<String> = list
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
        .map(String::from)
        .collect();
    if ids.is_empty() {
        return None;
    }

    let catalog: serde_json::Value = client
        .get(OPENCODE_CATALOG_URL)
        .timeout(std::time::Duration::from_secs(25))
        .send()
        .ok()?
        .json()
        .ok()?;
    let go_models = catalog
        .get("opencode-go")
        .and_then(|g| g.get("models"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let mut models = Vec::with_capacity(ids.len());
    for id in &ids {
        let m = go_models
            .get(id)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let context = m
            .get("limit")
            .and_then(|l| l.get("context"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let mut efforts = String::new();
        if let Some(options) = m.get("reasoning_options").and_then(|o| o.as_array()) {
            for opt in options {
                if opt.get("type").and_then(|t| t.as_str()) == Some("effort") {
                    let values: Vec<&str> = opt
                        .get("values")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .filter(|v| CLAUDE_EFFORTS.contains(v))
                                .collect()
                        })
                        .unwrap_or_default();
                    efforts = values.join(",");
                    break;
                }
            }
        }
        models.push(Model {
            slug: id.clone(),
            display: id.clone(),
            context,
            efforts,
            default: String::new(),
        });
    }
    Some(models)
}

/// openai subscription backend: models from the Codex CLI cache
/// (`~/.codex/models_cache.json`). Context = max_context_window ×
/// effective_context_window_percent (95%), i.e. the extended window the
/// backend supports.
pub fn fetch_openai_subscription_models(cache_path: &Path) -> Option<Vec<Model>> {
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache_path).ok()?).ok()?;
    let mut models = Vec::new();
    for m in data.get("models")?.as_array()? {
        let slug = m.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        if slug.is_empty() || slug == "codex-auto-review" {
            continue;
        }
        let display = m
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(slug)
            .to_string();
        let max_window = m
            .get("max_context_window")
            .or_else(|| m.get("context_window"));
        let eff_pct = m.get("effective_context_window_percent");
        let context = match (
            max_window.and_then(|v| v.as_u64()),
            eff_pct.and_then(|v| v.as_u64()),
        ) {
            (Some(mw), Some(eff)) => mw * eff / 100,
            _ => 0,
        };
        let efforts: Vec<&str> = m
            .get("supported_reasoning_levels")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("effort").and_then(|v| v.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let default = m
            .get("default_reasoning_level")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        models.push(Model {
            slug: slug.to_string(),
            display,
            context,
            efforts: efforts.join(","),
            default: default.to_string(),
        });
    }
    Some(models)
}

/// openai apikey backend: models from `api.openai.com/v1/models`, filtered to
/// chat-capable ids (same regexes as the bash script).
pub fn fetch_openai_apikey_models(api_key: &str) -> Option<Vec<Model>> {
    let client = json_client();
    let resp = client
        .get("https://api.openai.com/v1/models")
        .timeout(std::time::Duration::from_secs(20))
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let list: serde_json::Value = resp.json().ok()?;
    let mut models = Vec::new();
    for m in list.get("data")?.as_array()? {
        let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !is_apikey_model(id) {
            continue;
        }
        models.push(Model {
            slug: id.to_string(),
            display: id.to_string(),
            context: 0,
            efforts: "low,medium,high".to_string(),
            default: "medium".to_string(),
        });
    }
    Some(models)
}

/// The bash filter for apikey models: `^gpt-|^o[0-9]` minus the excluded
/// substrings.
fn is_apikey_model(id: &str) -> bool {
    let base = if let Some(rest) = id.strip_prefix("gpt-") {
        !rest.is_empty()
    } else if let Some(rest) = id.strip_prefix('o') {
        rest.chars().next().is_some_and(|c| c.is_ascii_digit())
    } else {
        false
    };
    if !base {
        return false;
    }
    let excluded = [
        "realtime",
        "audio",
        "image",
        "whisper",
        "tts",
        "embed",
        "dall",
        "moderation",
        "vision",
        "codex",
    ];
    !excluded.iter().any(|s| id.contains(s))
}

/// Static fallback list for the openai backend (used when no list is
/// available: no cache file, no API response).
pub fn openai_fallback_models() -> Vec<Model> {
    OPENAI_FALLBACK_MODELS
        .iter()
        .map(|(slug, efforts, default)| Model {
            slug: slug.to_string(),
            display: slug.to_string(),
            context: 0,
            efforts: efforts.join(","),
            default: default.to_string(),
        })
        .collect()
}

/// The single fallback model for the opencode backend.
pub fn opencode_fallback_models() -> Vec<Model> {
    vec![Model {
        slug: OPENCODE_FALLBACK_MODEL.to_string(),
        display: OPENCODE_FALLBACK_MODEL.to_string(),
        context: 0,
        efforts: String::new(),
        default: String::new(),
    }]
}

// ── Cache ──────────────────────────────────────────────────────────────────────

/// Reads the models.tsv cache; None when missing or empty.
pub fn read_models_cache(path: &Path) -> Option<Vec<Model>> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(parse_tsv(&text))
}

/// Age in seconds of the cache file (0 when missing).
pub fn cache_age(path: &Path) -> u64 {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        now_unix().saturating_sub(meta.mtime() as u64)
    }
    #[cfg(not(unix))]
    {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| now_unix().saturating_sub(d.as_secs()))
            .unwrap_or(0)
    }
}

/// Builds the models.tsv cache atomically (and the models.ids file with the
/// plain id list, like the bash script). Returns false on failure.
pub fn build_models_cache(models_path: &Path, ids_path: &Path, models: &[Model]) -> bool {
    let tsv = serialize_tsv(models);
    let tmp = models_path.with_extension("tmp.tsv");
    let Ok(mut f) = std::fs::File::create(&tmp) else {
        return false;
    };
    if f.write_all(tsv.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if crate::state::replace_file(&tmp, models_path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    let ids: String = models
        .iter()
        .map(|m| m.slug.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let _ = std::fs::write(ids_path, ids);
    true
}

/// The cache file path for a backend state dir (models.tsv).
pub fn models_cache_path(backend_dir: &Path) -> PathBuf {
    backend_dir.join("models.tsv")
}

/// The ids file path for a backend state dir (models.ids).
pub fn models_ids_path(backend_dir: &Path) -> PathBuf {
    backend_dir.join("models.ids")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsv_round_trips() {
        let models = vec![
            Model {
                slug: "minimax-m3".into(),
                display: "minimax-m3".into(),
                context: 1_000_000,
                efforts: String::new(),
                default: String::new(),
            },
            Model {
                slug: "gpt-5.6-sol".into(),
                display: "GPT-5.6 Sol".into(),
                context: 0,
                efforts: "low,medium,high".into(),
                default: "medium".into(),
            },
        ];
        let tsv = serialize_tsv(&models);
        assert_eq!(
            tsv,
            "minimax-m3\tminimax-m3\t1000000\t\t\ngpt-5.6-sol\tGPT-5.6 Sol\t\tlow,medium,high\tmedium\n"
        );
        assert_eq!(parse_tsv(&tsv), models);
        // Empty lines and blank fields tolerated.
        assert_eq!(parse_tsv("\n\t\nx\ty\n").len(), 1);
    }

    #[test]
    fn parses_the_bash_cache_format() {
        // A line the bash script could write: unknown context stays empty.
        let tsv = "hy3\t\t\t\t\n";
        let models = parse_tsv(tsv);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "hy3");
        assert_eq!(models[0].display, "hy3"); // falls back to the slug
        assert_eq!(models[0].context, 0);
    }

    #[test]
    fn filters_apikey_models_like_the_bash() {
        assert!(is_apikey_model("gpt-5.6-sol"));
        assert!(is_apikey_model("o3"));
        assert!(is_apikey_model("gpt-4o"));
        assert!(!is_apikey_model("gpt-4o-realtime"));
        assert!(!is_apikey_model("text-embedding-3-small"));
        assert!(!is_apikey_model("whisper-1"));
        assert!(!is_apikey_model("dall-e-3"));
        assert!(!is_apikey_model("gpt-4o-vision-preview"));
        assert!(!is_apikey_model("codex-latest"));
        assert!(!is_apikey_model("random"));
        assert!(!is_apikey_model(""));
    }

    #[test]
    fn fallback_lists_are_well_formed() {
        let fallback = openai_fallback_models();
        assert!(fallback.len() >= 6);
        assert!(fallback
            .iter()
            .all(|m| !m.slug.is_empty() && !m.efforts.is_empty()));
        let oc = opencode_fallback_models();
        assert_eq!(oc.len(), 1);
        assert_eq!(oc[0].slug, OPENCODE_FALLBACK_MODEL);
    }

    #[test]
    fn parses_the_codex_models_cache() {
        let dir = std::env::temp_dir().join(format!("opencc-models-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models_cache.json");
        std::fs::write(
            &path,
            r#"{"models": [
                {"slug": "gpt-5.6-sol", "display_name": "GPT-5.6 Sol", "max_context_window": 872000, "effective_context_window_percent": 95,
                 "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}], "default_reasoning_level": "low"},
                {"slug": "codex-auto-review"},
                {"slug": ""}
            ]}"#,
        )
        .unwrap();
        let models = fetch_openai_subscription_models(&path).expect("cache parses");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "gpt-5.6-sol");
        assert_eq!(models[0].display, "GPT-5.6 Sol");
        assert_eq!(models[0].context, 872000 * 95 / 100);
        assert_eq!(models[0].efforts, "low,high");
        assert_eq!(models[0].default, "low");
        assert!(fetch_openai_subscription_models(&dir.join("missing.json")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_the_opencode_catalog_shape() {
        // Rebuild what fetch_opencode_models would read from the catalog,
        // exercising the same JSON-shape logic.
        let catalog: serde_json::Value = serde_json::from_str(
            r#"{"opencode-go": {"models": {
                "minimax-m3": {"limit": {"context": 1000000}, "reasoning_options": []},
                "kimi-k3": {"limit": {"context": 262144},
                            "reasoning_options": [{"type": "effort", "values": ["low", "high", "max", "turbo"]}]}
            }}}"#,
        )
        .unwrap();
        let go_models = catalog.get("opencode-go").unwrap().get("models").unwrap();
        let m3 = go_models.get("minimax-m3").unwrap();
        let ctx = m3
            .get("limit")
            .unwrap()
            .get("context")
            .unwrap()
            .as_u64()
            .unwrap();
        assert_eq!(ctx, 1_000_000);
        let k3 = go_models.get("kimi-k3").unwrap();
        let values: Vec<&str> = k3
            .get("reasoning_options")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o.get("type").and_then(|t| t.as_str()) == Some("effort"))
            .flat_map(|o| {
                o.get("values")
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|v| CLAUDE_EFFORTS.contains(v))
                    .collect::<Vec<_>>()
            })
            .collect();
        // "turbo" is not accepted by Claude Code and is filtered out.
        assert_eq!(values, vec!["low", "high", "max"]);
    }

    #[test]
    fn cache_age_and_build() {
        let dir = std::env::temp_dir().join(format!("opencc-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let models_path = dir.join("models.tsv");
        let ids_path = dir.join("models.ids");
        let models = opencode_fallback_models();
        assert!(build_models_cache(&models_path, &ids_path, &models));
        assert_eq!(read_models_cache(&models_path).unwrap(), models);
        assert_eq!(std::fs::read_to_string(&ids_path).unwrap(), "minimax-m3\n");
        assert!(cache_age(&models_path) < 60);
        assert_eq!(cache_age(&dir.join("missing.tsv")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
