//! Classic numbered text menus, written against `BufRead`/`Write` so they
//! are testable with injected streams. They are the fallback when stdin is
//! not a terminal (pipes, scripts, CI); on a real terminal
//! [`crate::interactive`] runs the arrow-key/mouse pickers instead.

use std::io::{BufRead, Write};

/// Prints `prompt` and reads one line (empty on EOF).
pub fn prompt_line(prompt: &str, input: &mut impl BufRead, output: &mut impl Write) -> String {
    let _ = write!(output, "{prompt}");
    let _ = output.flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => String::new(),
        Ok(_) => line.trim().to_string(),
    }
}

/// `prompt_line` + yes/no: true only for y/Y.
pub fn ask_yes_no(prompt: &str, input: &mut impl BufRead, output: &mut impl Write) -> bool {
    matches!(prompt_line(prompt, input, output).as_str(), "y" | "Y")
}

/// The backend selection menu (1 = openai, 2 = opencode, 0 = anthropic).
/// Returns the chosen backend id. `default` is the last used backend.
pub fn choose_backend(input: &mut impl BufRead, output: &mut impl Write, default: &str) -> String {
    let _ = writeln!(output, "Available backends:");
    let _ = writeln!(
        output,
        "   1) openai  — OpenAI models (ChatGPT subscription or API key, local proxy)"
    );
    let _ = writeln!(output, "   2) opencode — OpenCode gateway (x-api-key)");
    let _ = writeln!(
        output,
        "   0) anthropic — stock Claude Code (pass-through, no changes)"
    );
    loop {
        let choice = prompt_line(
            &format!("\nChoose backend [0-2] (enter/d = {default}): "),
            input,
            output,
        );
        match choice.as_str() {
            "" | "d" => return default.to_string(),
            "0" | "anthropic" => return "anthropic".to_string(),
            "1" | "openai" => return "openai".to_string(),
            "2" | "opencode" | "go" => return "opencode".to_string(),
            _ => {
                let _ = writeln!(output, "Invalid choice, try again.");
            }
        }
    }
}

/// One-line model description shared by the text menu and the interactive
/// picker: `"GPT-5.6 Sol  [828K]  (last used)"`.
pub fn model_label(display: &str, context: u64, last_used: bool) -> String {
    let ctx = if context > 0 {
        format!("  [{}]", crate::util::fmt_ctx(context))
    } else {
        String::new()
    };
    let tag = if last_used { "  (last used)" } else { "" };
    format!("{display}{ctx}{tag}")
}

/// Prints the numbered model list. `last_used` gets a "(last used)" marker.
pub fn print_model_list(
    models: &[crate::models::Model],
    last_used: &str,
    header: &str,
    output: &mut impl Write,
) {
    let _ = writeln!(output, "{header}");
    for (i, m) in models.iter().enumerate() {
        let _ = writeln!(
            output,
            "  {:2}) {}",
            i + 1,
            model_label(&m.display, m.context, m.slug == last_used)
        );
    }
}

/// Chooses a model from the numbered list. Enter/d → the default.
pub fn choose_model(
    input: &mut impl BufRead,
    output: &mut impl Write,
    models: &[crate::models::Model],
    default: &str,
) -> String {
    loop {
        let choice = prompt_line(
            &format!(
                "\nChoose a model [1-{}] (enter/d = {default}): ",
                models.len()
            ),
            input,
            output,
        );
        if choice.is_empty() || choice == "d" {
            return default.to_string();
        }
        if let Ok(n) = choice.parse::<usize>() {
            if n >= 1 && n <= models.len() {
                return models[n - 1].slug.clone();
            }
        }
        let _ = writeln!(output, "Invalid choice, try again.");
    }
}

/// Chooses a reasoning level among `valid` (CSV). Enter → the default
/// (`last` if valid for this model, else the model's own default).
pub fn choose_effort(
    input: &mut impl BufRead,
    output: &mut impl Write,
    valid: &[String],
    last: &str,
    model_default: &str,
) -> String {
    let valid_csv = valid.join(",");
    let last_ok = !last.is_empty() && valid.iter().any(|v| v == last);
    let defprompt = if last_ok {
        last.to_string()
    } else if model_default.is_empty() {
        "model default".to_string()
    } else {
        format!("model default ({model_default})")
    };
    let effort = prompt_line(
        &format!("Reasoning level [{valid_csv}] (enter = {defprompt}): "),
        input,
        output,
    );
    if effort.is_empty() {
        return if last_ok {
            last.to_string()
        } else {
            String::new()
        };
    }
    if valid.iter().any(|v| v == &effort) {
        effort
    } else {
        let _ = writeln!(output, "Invalid level ('{effort}'); using the default.");
        if last_ok {
            last.to_string()
        } else {
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run<F: FnOnce(&mut Cursor<Vec<u8>>, &mut Cursor<Vec<u8>>) -> String>(
        input: &str,
        f: F,
    ) -> String {
        let mut i = Cursor::new(input.as_bytes().to_vec());
        let mut o = Cursor::new(Vec::new());
        f(&mut i, &mut o)
    }

    #[test]
    fn backend_menu_accepts_defaults_and_numbers() {
        assert_eq!(run("", |i, o| choose_backend(i, o, "openai")), "openai");
        assert_eq!(
            run("d\n", |i, o| choose_backend(i, o, "anthropic")),
            "anthropic"
        );
        assert_eq!(
            run("2\n", |i, o| choose_backend(i, o, "openai")),
            "opencode"
        );
        assert_eq!(
            run("go\n", |i, o| choose_backend(i, o, "openai")),
            "opencode"
        );
        assert_eq!(
            run("0\n", |i, o| choose_backend(i, o, "openai")),
            "anthropic"
        );
        assert_eq!(
            run("1\n", |i, o| choose_backend(i, o, "opencode")),
            "openai"
        );
        // Invalid choice retries.
        assert_eq!(
            run("9\n2\n", |i, o| choose_backend(i, o, "openai")),
            "opencode"
        );
    }

    #[test]
    fn model_menu_retries_on_bad_input() {
        let models = vec![
            crate::models::Model {
                slug: "a".into(),
                display: "A".into(),
                context: 828_400,
                efforts: String::new(),
                default: String::new(),
            },
            crate::models::Model {
                slug: "b".into(),
                display: "B".into(),
                context: 0,
                efforts: String::new(),
                default: String::new(),
            },
        ];
        assert_eq!(run("", |i, o| choose_model(i, o, &models, "a")), "a");
        assert_eq!(run("d\n", |i, o| choose_model(i, o, &models, "b")), "b");
        assert_eq!(run("2\n", |i, o| choose_model(i, o, &models, "a")), "b");
        assert_eq!(
            run("0\nx\n1\n", |i, o| choose_model(i, o, &models, "a")),
            "a"
        );
    }

    #[test]
    fn model_label_formats_context_and_marker() {
        assert_eq!(model_label("M", 828_400, false), "M  [828K]");
        assert_eq!(model_label("M", 828_400, true), "M  [828K]  (last used)");
        assert_eq!(model_label("M", 0, true), "M  (last used)");
        assert_eq!(model_label("M", 0, false), "M");
        assert_eq!(
            model_label("GPT-5.6 Sol", 1_000_000, true),
            "GPT-5.6 Sol  [1M]  (last used)"
        );
    }

    #[test]
    fn effort_menu_uses_last_or_model_default() {
        let valid: Vec<String> = ["low", "high"].iter().map(|s| s.to_string()).collect();
        // Empty answer + last used valid → last used.
        assert_eq!(
            run("", |i, o| choose_effort(i, o, &valid, "low", "high")),
            "low"
        );
        // Empty answer + last used invalid → no effort (the model's own
        // default applies upstream, like the bash script).
        assert_eq!(
            run("", |i, o| choose_effort(i, o, &valid, "max", "high")),
            ""
        );
        assert_eq!(run("", |i, o| choose_effort(i, o, &valid, "max", "")), "");
        // Invalid level → last used.
        assert_eq!(
            run("turbo\n", |i, o| choose_effort(i, o, &valid, "high", "low")),
            "high"
        );
        // Valid level chosen.
        assert_eq!(
            run("high\n", |i, o| choose_effort(i, o, &valid, "low", "high")),
            "high"
        );
    }
}
