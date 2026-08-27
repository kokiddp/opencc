//! Effort (reasoning level) normalization.
//!
//! Claude Code sends efforts `low..max` in `output_config.effort`; the models
//! behind the alternative backends have their own policies, read from
//! `model-efforts.json`. The proxy applies the model's real policy to the
//! request: unsupported levels are clamped down to the highest available level
//! not exceeding the requested one; if none is lower, to the lowest available.
//!
//! Direct port of `normalizeEffort` from the node proxy.

/// Levels accepted by Claude Code's `--effort` / `/effort`.
pub const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Total order of the levels, including `ultra` (which only exists encoded in
/// the model as `model@ultra`, because `/effort` does not accept it).
pub const EFFORT_ORDER: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultra"];

/// The effort policy of a model, as read from `model-efforts.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortPolicy {
    pub supported: Vec<String>,
    pub default: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct EffortDecision {
    /// The effort requested by the client (or None if none was sent).
    pub requested: Option<String>,
    /// The effort actually applied (None = removed).
    pub applied: Option<String>,
    /// Why: exact | clamped | default | unknown-level | unsupported-model |
    /// no-effort | no-policy.
    pub reason: &'static str,
}

/// Parses a model spec `"gpt-5.6-sol@high"` → `{ id, effort }`. The historical
/// `model@effort` suffix stays supported and takes precedence over
/// `output_config.effort` for compatibility.
pub fn parse_model_spec(model: &str) -> ModelSpec {
    match model.split_once('@') {
        Some((id, effort)) => ModelSpec {
            id: id.to_string(),
            effort: Some(effort.to_string()),
        },
        None => ModelSpec {
            id: model.to_string(),
            effort: None,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: String,
    pub effort: Option<String>,
}

/// Applies the model's policy to the requested effort.
pub fn normalize_effort(requested: Option<&str>, policy: Option<&EffortPolicy>) -> EffortDecision {
    let req = requested.map(|s| s.to_string());
    let Some(policy) = policy else {
        return EffortDecision {
            requested: req.clone(),
            applied: req,
            reason: "no-policy",
        };
    };

    if policy.supported.is_empty() {
        return EffortDecision {
            requested: req,
            applied: None,
            reason: if requested.is_some() {
                "unsupported-model"
            } else {
                "no-effort"
            },
        };
    }

    let Some(requested) = requested else {
        return EffortDecision {
            requested: None,
            applied: policy.default.clone(),
            reason: "default",
        };
    };

    if policy.supported.iter().any(|v| v == requested) {
        return EffortDecision {
            requested: req.clone(),
            applied: req,
            reason: "exact",
        };
    }

    let requested_rank = EFFORT_ORDER.iter().position(|v| *v == requested);
    let Some(requested_rank) = requested_rank else {
        return EffortDecision {
            requested: req,
            applied: policy
                .default
                .clone()
                .or_else(|| policy.supported.first().cloned()),
            reason: "unknown-level",
        };
    };

    // Clamp: highest available level not exceeding the requested one; if none
    // is lower, the lowest available one.
    let ranked: Vec<(String, usize)> = policy
        .supported
        .iter()
        .filter_map(|v| {
            EFFORT_ORDER
                .iter()
                .position(|e| e == v)
                .map(|rank| (v.clone(), rank))
        })
        .collect();
    let below = ranked
        .iter()
        .filter(|(_, rank)| *rank <= requested_rank)
        .max_by_key(|(_, rank)| *rank);
    let applied = below
        .map(|(v, _)| v.clone())
        .or_else(|| ranked.first().map(|(v, _)| v.clone()));
    EffortDecision {
        requested: req,
        applied,
        reason: "clamped",
    }
}

/// Reads the effort policy for a model from a `model-efforts.json` file.
/// The file is read on every call (like the node proxy) so policy changes
/// apply without a restart.
pub fn read_effort_policy(path: &std::path::Path, model: &str) -> Option<EffortPolicy> {
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let policy = data.get("models")?.get(model)?;
    let supported: Vec<String> = policy
        .get("supported")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|v| EFFORT_ORDER.contains(v))
        .map(|v| v.to_string())
        .collect();
    let default = policy
        .get("default")
        .and_then(|v| v.as_str())
        .filter(|d| supported.iter().any(|s| s == d))
        .map(|s| s.to_string());
    Some(EffortPolicy { supported, default })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(supported: &[&str], default: Option<&str>) -> EffortPolicy {
        EffortPolicy {
            supported: supported.iter().map(|s| s.to_string()).collect(),
            default: default.map(|s| s.to_string()),
        }
    }

    #[test]
    fn parses_model_specs() {
        assert_eq!(
            parse_model_spec("gpt-5.6-sol@high"),
            ModelSpec {
                id: "gpt-5.6-sol".into(),
                effort: Some("high".into())
            }
        );
        assert_eq!(
            parse_model_spec("minimax-m3"),
            ModelSpec {
                id: "minimax-m3".into(),
                effort: None
            }
        );
        assert_eq!(
            parse_model_spec("m@"),
            ModelSpec {
                id: "m".into(),
                effort: Some("".into())
            }
        );
    }

    #[test]
    fn normalizes_against_the_real_capabilities() {
        let sparse = policy(&["low", "high", "max"], Some("high"));
        assert_eq!(
            normalize_effort(Some("high"), Some(&sparse)),
            EffortDecision {
                requested: Some("high".into()),
                applied: Some("high".into()),
                reason: "exact"
            }
        );
        assert_eq!(
            normalize_effort(Some("medium"), Some(&sparse)),
            EffortDecision {
                requested: Some("medium".into()),
                applied: Some("low".into()),
                reason: "clamped"
            }
        );
        assert_eq!(
            normalize_effort(Some("xhigh"), Some(&sparse)),
            EffortDecision {
                requested: Some("xhigh".into()),
                applied: Some("high".into()),
                reason: "clamped"
            }
        );
        assert_eq!(
            normalize_effort(None, Some(&sparse)),
            EffortDecision {
                requested: None,
                applied: Some("high".into()),
                reason: "default"
            }
        );
    }

    #[test]
    fn clamps_to_lowest_when_nothing_is_below() {
        // Requested "low", policy has only "high": fall back to the lowest.
        let p = policy(&["high"], Some("high"));
        assert_eq!(
            normalize_effort(Some("low"), Some(&p)),
            EffortDecision {
                requested: Some("low".into()),
                applied: Some("high".into()),
                reason: "clamped"
            }
        );
    }

    #[test]
    fn removes_effort_for_models_without_it() {
        let p = policy(&[], None);
        assert_eq!(
            normalize_effort(Some("max"), Some(&p)),
            EffortDecision {
                requested: Some("max".into()),
                applied: None,
                reason: "unsupported-model"
            }
        );
        assert_eq!(
            normalize_effort(None, Some(&p)),
            EffortDecision {
                requested: None,
                applied: None,
                reason: "no-effort"
            }
        );
    }

    #[test]
    fn unknown_level_uses_the_default() {
        let p = policy(&["low", "high"], Some("low"));
        assert_eq!(
            normalize_effort(Some("turbo"), Some(&p)),
            EffortDecision {
                requested: Some("turbo".into()),
                applied: Some("low".into()),
                reason: "unknown-level"
            }
        );
    }

    #[test]
    fn no_policy_passes_through() {
        assert_eq!(
            normalize_effort(Some("high"), None),
            EffortDecision {
                requested: Some("high".into()),
                applied: Some("high".into()),
                reason: "no-policy"
            }
        );
        assert_eq!(
            normalize_effort(None, None),
            EffortDecision {
                requested: None,
                applied: None,
                reason: "no-policy"
            }
        );
    }

    #[test]
    fn reads_the_policy_file() {
        let dir = std::env::temp_dir().join(format!("opencc-effort-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model-efforts.json");
        std::fs::write(
            &path,
            r#"{"models": {"gpt-one": {"supported": [], "default": null},
                           "gpt-two": {"supported": ["low", "medium", "high", "turbo"], "default": "medium"}}}"#,
        )
        .unwrap();

        let none = read_effort_policy(&path, "gpt-one").expect("empty policy reads");
        assert!(none.supported.is_empty());

        // "turbo" is not a real level and must be filtered out.
        let p = read_effort_policy(&path, "gpt-two").expect("policy reads");
        assert_eq!(p.supported, vec!["low", "medium", "high"]);
        assert_eq!(p.default.as_deref(), Some("medium"));

        assert!(read_effort_policy(&path, "gpt-unknown").is_none());
        assert!(read_effort_policy(&dir.join("missing.json"), "gpt-one").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
