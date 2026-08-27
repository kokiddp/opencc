//! Generation of the Claude Code `--settings` files:
//!
//! - `model-picker.json`: a `modelPicker` entry with `replaceBuiltInOptions`,
//!   so `/model` shows the backend models (arbitrary IDs — Claude Code drops
//!   by design every ID that does not contain `claude`/`anthropic` from
//!   automatic discovery).
//! - `model-efforts.json`: the per-model effort policy the proxy applies.

use crate::models::{efforts_csv, Model};
use serde_json::{json, Map, Value};

pub struct PickerFiles {
    pub picker: Value,
    pub policy: Value,
}

/// Builds both documents from the model list.
///
/// Direct port of the bash/python generator, except the description for
/// models without configurable effort is the English "effort: not
/// configurable" (the bash had an Italian leftover, "non configurabile").
pub fn generate_picker_and_policy(models: &[Model], base_description: &str) -> PickerFiles {
    let mut options = Vec::new();
    let mut policies = Map::new();

    for m in models {
        let supported = efforts_csv(&m.efforts);
        let default = if supported.contains(&m.default) {
            Some(m.default.clone())
        } else {
            None
        };
        let effort_description = if supported.is_empty() {
            "effort: not configurable".to_string()
        } else {
            let mut d = format!("effort: {}", supported.join(", "));
            if let Some(def) = &default {
                d.push_str(&format!(" (default: {def})"));
            }
            d
        };
        options.push(json!({
            "model": m.slug,
            "label": m.display,
            "description": format!("{base_description} · {effort_description}"),
        }));
        policies.insert(
            m.slug.clone(),
            json!({
                "supported": supported,
                "default": default,
            }),
        );
    }

    PickerFiles {
        picker: json!({
            "modelPicker": {
                "replaceBuiltInOptions": true,
                "options": options,
            }
        }),
        policy: json!({ "models": Value::Object(policies) }),
    }
}

/// Writes both files atomically, returning their paths.
pub fn write_picker_files(
    state_dir: &std::path::Path,
    models: &[Model],
    base_description: &str,
) -> std::io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let files = generate_picker_and_policy(models, base_description);
    let picker_path = state_dir.join("model-picker.json");
    let policy_path = state_dir.join("model-efforts.json");
    crate::state::write_atomic_text(&picker_path, &serde_json::to_string_pretty(&files.picker)?)?;
    crate::state::write_atomic_text(&policy_path, &serde_json::to_string_pretty(&files.policy)?)?;
    Ok((picker_path, policy_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Model;

    fn model(slug: &str, efforts: &str, default: &str) -> Model {
        Model {
            slug: slug.into(),
            display: slug.into(),
            context: 0,
            efforts: efforts.into(),
            default: default.into(),
        }
    }

    #[test]
    fn generates_the_picker_and_policy() {
        let models = vec![
            model("minimax-m3", "", ""),
            model("kimi-k3", "low,high,max", "high"),
            model("glm-5", "low,medium,high", "turbo"), // invalid default → None
        ];
        let files = generate_picker_and_policy(&models, "OpenCode via opencc");

        let options = files.picker["modelPicker"]["options"].as_array().unwrap();
        assert_eq!(options.len(), 3);
        assert_eq!(
            files.picker["modelPicker"]["replaceBuiltInOptions"],
            json!(true)
        );

        assert_eq!(
            options[0]["description"],
            "OpenCode via opencc · effort: not configurable"
        );
        assert_eq!(
            options[1]["description"],
            "OpenCode via opencc · effort: low, high, max (default: high)"
        );
        assert_eq!(
            options[2]["description"],
            "OpenCode via opencc · effort: low, medium, high"
        );

        let policies = files.policy["models"].as_object().unwrap();
        assert_eq!(
            policies["minimax-m3"],
            json!({"supported": [], "default": null})
        );
        assert_eq!(
            policies["kimi-k3"],
            json!({"supported": ["low", "high", "max"], "default": "high"})
        );
        assert_eq!(
            policies["glm-5"],
            json!({"supported": ["low", "medium", "high"], "default": null})
        );
    }

    #[test]
    fn writes_both_files_atomically() {
        let dir = std::env::temp_dir().join(format!("opencc-picker-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (picker, policy) = write_picker_files(
            &dir,
            &[model("gpt-one", "low,medium", "medium")],
            "OpenAI via opencc (apikey)",
        )
        .unwrap();
        let picker_json: Value =
            serde_json::from_str(&std::fs::read_to_string(&picker).unwrap()).unwrap();
        assert_eq!(picker_json["modelPicker"]["options"][0]["model"], "gpt-one");
        let policy_json: Value =
            serde_json::from_str(&std::fs::read_to_string(&policy).unwrap()).unwrap();
        assert_eq!(policy_json["models"]["gpt-one"]["default"], "medium");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
