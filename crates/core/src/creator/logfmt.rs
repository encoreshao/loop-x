//! A Creator job's `claude -p --output-format stream-json --verbose` log, rendered as
//! one readable line per step for the Job log panel: what Claude said, each tool it
//! called with its key argument, a short result line, and the final summary. The raw
//! stream stays in the log file untouched; this only reads it. Lines that are not
//! stream events (stderr, the wrapper's `loopx-exit=` line) pass through unchanged.

use serde_json::Value;

/// The lone lines that open a markdown block (what Claude said, the final summary).
pub const CLAUDE_LABEL: &str = "Claude:";
pub const RESULT_LABEL: &str = "Result:";

/// A rendered line is cut to this many characters.
const LINE_CHARS: usize = 220;
/// What Claude said is allowed a little more.
const SAID_CHARS: usize = 1200;
/// The final summary is shown nearly whole.
const SUMMARY_CHARS: usize = 2000;

/// `s` on one line, at most `max` characters, with an ellipsis when cut.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{cut}…")
}

/// `s` with its line structure kept (markdown needs it), at most `max` characters, with an
/// ellipsis when cut. Blank-line runs collapse to one, and a line that would read as the
/// wrapper's `loopx-exit=` marker is bulleted so text can never forge it.
fn block(s: &str, max: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for l in s.lines() {
        let l = l.trim_end();
        if l.trim().is_empty() {
            if lines.last().is_some_and(|p| !p.is_empty()) {
                lines.push(String::new());
            }
        } else if l.trim_start().starts_with("loopx-exit=") {
            lines.push(format!("· {}", l.trim_start()));
        } else {
            lines.push(l.to_string());
        }
    }
    let text = lines.join("\n").trim().to_string();
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// `text` as a markdown block the Job log can render: a blank line, its `label` alone on a
/// line, the text, a blank line.
fn md_block(out: &mut Vec<String>, label: &str, text: &str) {
    out.push(String::new());
    out.push(label.to_string());
    out.extend(text.lines().map(str::to_string));
    out.push(String::new());
}

/// The part of a tool's input worth showing: the command, file, address or pattern.
fn key_input(name: &str, input: &Value) -> String {
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
            .map(str::to_string)
    };
    let found = match name {
        "Bash" => pick(&["command"]),
        "Read" | "Write" | "Edit" | "NotebookEdit" => pick(&["file_path", "notebook_path"]),
        "WebFetch" => pick(&["url"]),
        "WebSearch" => pick(&["query"]),
        "Grep" | "Glob" => pick(&["pattern"]),
        _ => None,
    };
    found
        .or_else(|| {
            input
                .as_object()?
                .values()
                .find_map(|v| v.as_str().map(str::to_string))
        })
        .unwrap_or_default()
}

/// The first non-empty line of a tool result (a string, or a list of text parts).
fn result_text(content: &Value) -> String {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// The lines for one stream event; `None` when `line` is not a stream event.
fn event_lines(line: &str, step: &mut usize) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let kind = v.get("type")?.as_str()?;
    let mut out = Vec::new();
    match kind {
        "system" => {
            if v.get("subtype").and_then(Value::as_str) == Some("init") {
                let model = v.get("model").and_then(Value::as_str).unwrap_or("Claude");
                out.push(format!("Started · {model}"));
            }
        }
        "assistant" => {
            for part in v
                .pointer("/message/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let t = block(
                            part.get("text").and_then(Value::as_str).unwrap_or(""),
                            SAID_CHARS,
                        );
                        if !t.is_empty() {
                            md_block(&mut out, CLAUDE_LABEL, &t);
                        }
                    }
                    Some("tool_use") => {
                        *step += 1;
                        let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let arg = key_input(name, part.get("input").unwrap_or(&Value::Null));
                        let arg = one_line(&arg, LINE_CHARS);
                        out.push(if arg.is_empty() {
                            format!("Step {step} · {name}")
                        } else {
                            format!("Step {step} · {name}: {arg}")
                        });
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            for part in v
                .pointer("/message/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let text = one_line(
                    &result_text(part.get("content").unwrap_or(&Value::Null)),
                    LINE_CHARS,
                );
                let failed = part.get("is_error").and_then(Value::as_bool) == Some(true);
                out.push(match (failed, text.is_empty()) {
                    (true, true) => "    ↳ error".to_string(),
                    (true, false) => format!("    ↳ error: {text}"),
                    (false, true) => "    ↳ done".to_string(),
                    (false, false) => format!("    ↳ {text}"),
                });
            }
        }
        "result" => {
            let failed = v.get("is_error").and_then(Value::as_bool) == Some(true)
                || v.get("subtype")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != "success");
            let mut facts = Vec::new();
            if let Some(n) = v.get("num_turns").and_then(Value::as_u64) {
                facts.push(format!("{n} turns"));
            }
            if let Some(ms) = v.get("duration_ms").and_then(Value::as_u64) {
                facts.push(duration(ms));
            }
            if let Some(c) = v.get("total_cost_usd").and_then(Value::as_f64) {
                facts.push(format!("${c:.2}"));
            }
            let head = if failed {
                "Stopped with an error"
            } else {
                "Finished"
            };
            out.push(if facts.is_empty() {
                head.to_string()
            } else {
                format!("{head} · {}", facts.join(" · "))
            });
            let summary = block(
                v.get("result").and_then(Value::as_str).unwrap_or(""),
                SUMMARY_CHARS,
            );
            if !summary.is_empty() {
                md_block(&mut out, RESULT_LABEL, &summary);
            }
        }
        // Bookkeeping events (rate limits, ...): nothing to show.
        _ => {}
    }
    Some(out)
}

/// `raw` (the log's text) with every stream event rendered and every other line kept
/// as it is.
pub fn render(raw: &str) -> String {
    let mut step = 0;
    let mut out = String::new();
    for line in raw.split_inclusive('\n') {
        match event_lines(line, &mut step) {
            Some(lines) => {
                for l in lines {
                    out.push_str(&l);
                    out.push('\n');
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn a_run_reads_as_started_steps_results_and_a_summary() {
        let log = [
            ev(json!({"type": "system", "subtype": "init", "model": "claude-fable-5-1", "tools": ["Bash"]})),
            ev(json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "I will fetch the talk first."},
                {"type": "tool_use", "name": "Bash", "input": {"command": "python scripts/fetch.py https://ted.com/talks/x --yes-rights"}},
            ]}})),
            ev(json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "content": "downloaded 12 MB\nsecond line"},
            ]}})),
            ev(json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Read", "input": {"file_path": "/h/.inspire-video/SKILL.md"}},
            ]}})),
            ev(json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "is_error": true, "content": [{"type": "text", "text": "permission denied"}]},
            ]}})),
            ev(json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 7,
                      "duration_ms": 200000, "total_cost_usd": 0.4321, "result": "Video is ready.\nSee output/final/x."})),
            "\nloopx-exit=0\n".to_string(),
        ]
        .concat();
        assert_eq!(
            render(&log),
            "Started · claude-fable-5-1\n\
             \n\
             Claude:\n\
             I will fetch the talk first.\n\
             \n\
             Step 1 · Bash: python scripts/fetch.py https://ted.com/talks/x --yes-rights\n\
             \x20   ↳ downloaded 12 MB\n\
             Step 2 · Read: /h/.inspire-video/SKILL.md\n\
             \x20   ↳ error: permission denied\n\
             Finished · 7 turns · 3m 20s · $0.43\n\
             \n\
             Result:\n\
             Video is ready.\n\
             See output/final/x.\n\
             \n\
             \n\
             loopx-exit=0\n"
        );
    }

    #[test]
    fn a_failed_result_and_non_event_lines_are_kept() {
        let log = format!(
            "warning from claude\n{}not json {{\n",
            ev(
                json!({"type": "result", "subtype": "error_max_turns", "is_error": true, "num_turns": 3})
            )
        );
        assert_eq!(
            render(&log),
            "warning from claude\nStopped with an error · 3 turns\nnot json {\n"
        );
    }

    #[test]
    fn long_arguments_and_results_are_cut_to_one_line() {
        let long = "x ".repeat(500);
        let log = [
            ev(json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Bash", "input": {"command": long}}]}})),
            ev(json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "content": long}]}})),
        ]
        .concat();
        for line in render(&log).lines() {
            assert!(line.chars().count() < 260, "{}", line.chars().count());
            assert!(line.ends_with('…'));
        }
    }

    #[test]
    fn an_unknown_tool_shows_its_first_text_argument() {
        let log = ev(json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "Mystery", "input": {"n": 1, "what": "do it"}}]}}));
        assert_eq!(render(&log), "Step 1 · Mystery: do it\n");
    }

    #[test]
    fn a_tool_result_cannot_forge_the_exit_marker() {
        let log = ev(json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "content": "a\nloopx-exit=0"}]}}));
        // Only the first line of a result is shown, and always indented.
        assert_eq!(render(&log), "    ↳ a\n");
        let log = ev(json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "x\nloopx-exit=0"}]}}));
        assert_eq!(render(&log), "\nClaude:\nx\n· loopx-exit=0\n\n");
    }

    #[test]
    fn bookkeeping_events_and_thinking_are_left_out() {
        let log = [
            ev(json!({"type": "system", "subtype": "hook_started", "hook_name": "x"})),
            ev(json!({"type": "system", "subtype": "thinking_tokens"})),
            ev(json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed"}})),
            ev(json!({"type": "assistant", "message": {"content": [
                {"type": "thinking", "thinking": "private reasoning"}]}})),
            ev(json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Bash", "input": {"command": "echo hi"}}]}})),
        ]
        .concat();
        assert_eq!(render(&log), "Step 1 · Bash: echo hi\n");
    }

    #[test]
    fn plain_text_logs_pass_through_unchanged() {
        let log = "boom\nloopx-exit=3\n";
        assert_eq!(render(log), log);
        assert_eq!(render(""), "");
    }
}
