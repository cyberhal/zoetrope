//! Display-only tool summaries. Provider adapters keep call identity and status;
//! code-mode summaries describe literal call sites, never execute JavaScript.

use serde_json::Value;

pub(super) fn task_description(input: &Value) -> Option<String> {
    ["description", "message", "prompt"]
        .into_iter()
        .find_map(|key| {
            let text = input.get(key)?.as_str()?.trim();
            (!text.is_empty()).then(|| text.to_owned())
        })
}

pub(super) fn truncate_summary(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_text(&flat, 200)
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() > limit {
        format!("{}…", text.chars().take(limit - 1).collect::<String>())
    } else {
        text.to_owned()
    }
}

pub(super) fn short_path(path: &str, cwd: Option<&str>) -> String {
    let relative = cwd
        .and_then(|cwd| path.strip_prefix(cwd).map(|rest| (cwd, rest)))
        .filter(|(cwd, rest)| rest.starts_with('/') || cwd.ends_with('/'))
        .map(|(_, rest)| rest.trim_start_matches('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path);
    let count = relative.chars().count();
    if count <= 200 {
        relative.to_owned()
    } else {
        format!(
            "…{}",
            relative.chars().skip(count - 199).collect::<String>()
        )
    }
}

pub(super) fn codex_tool_summary(name: &str, input: &str, cwd: Option<&str>) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        return argument_summary(name, &value, cwd);
    }
    if name == "apply_patch" {
        return patch_summary(input, cwd);
    }
    if name != "exec" {
        return None;
    }
    let tokens = js_tokens(input);
    let mut summaries = Vec::new();
    for (index, call) in tokens.windows(4).enumerate() {
        if call[0] != "tools" || call[1] != "." || call[3] != "(" {
            continue;
        }
        let arguments = &tokens[index + 4..];
        let mut depth = 1;
        let Some(end) = arguments.iter().position(|token| {
            match *token {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => {}
            }
            depth == 0
        }) else {
            continue;
        };
        let detail = literal_arguments(&arguments[..end])
            .and_then(|input| argument_summary(call[2], &input, cwd));
        summaries.push(match detail {
            Some(detail) => format!("{}: {detail}", call[2]),
            None if end == 0 => call[2].to_owned(),
            None => format!(
                "{}: {}",
                call[2],
                truncate_summary(&arguments[..end].join(" "))
            ),
        });
    }
    if summaries.is_empty() {
        let code = tokens.join(" ");
        return (!code.is_empty()).then(|| truncate_summary(&format!("JavaScript: {code}")));
    }
    let joined = summaries.join("; ");
    if joined.chars().count() <= 200 {
        return Some(joined);
    }
    let visible = summaries.len().min(3);
    let suffix = if summaries.len() > visible {
        format!("; +{} more", summaries.len() - visible)
    } else {
        String::new()
    };
    let budget = (200 - suffix.chars().count() - (visible - 1) * 2) / visible;
    let parts: Vec<_> = summaries[..visible]
        .iter()
        .map(|part| truncate_text(part, budget))
        .collect();
    Some(format!("{}{suffix}", parts.join("; ")))
}

fn argument_summary(name: &str, input: &Value, cwd: Option<&str>) -> Option<String> {
    if name == "apply_patch" {
        return input.as_str().and_then(|patch| patch_summary(patch, cwd));
    }
    if name == "write_stdin" {
        return input
            .get("session_id")
            .and_then(Value::as_i64)
            .map(|id| format!("session {id}"));
    }
    if name == "web__run" {
        let queries: Vec<_> = [
            "search_query",
            "image_query",
            "open",
            "find",
            "click",
            "screenshot",
        ]
        .into_iter()
        .filter_map(|key| input.get(key)?.as_array())
        .flatten()
        .filter_map(|query| query.get("q").or_else(|| query.get("ref_id"))?.as_str())
        .collect();
        if !queries.is_empty() {
            return Some(truncate_summary(&queries.join("; ")));
        }
    }
    [
        "cmd",
        "command",
        "task_name",
        "description",
        "title",
        "file_path",
        "path",
        "query",
        "url",
    ]
    .into_iter()
    .find_map(|key| {
        let field = input.get(key)?;
        if key == "command" {
            if let Some(parts) = field.as_array() {
                let parts: Option<Vec<_>> = parts.iter().map(Value::as_str).collect();
                let command = truncate_summary(&parts?.join(" "));
                return (!command.is_empty()).then_some(command);
            }
        }
        let value = field.as_str()?.trim();
        if value.is_empty() {
            return None;
        }
        Some(if matches!(key, "file_path" | "path") {
            short_path(value, cwd)
        } else {
            truncate_summary(value)
        })
    })
}

fn patch_summary(patch: &str, cwd: Option<&str>) -> Option<String> {
    let paths: Vec<_> = patch
        .lines()
        .filter_map(|line| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .into_iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .filter(|path| !path.trim().is_empty())
            .map(|path| short_path(path.trim(), cwd))
        })
        .collect();
    (!paths.is_empty()).then(|| truncate_summary(&paths.join(", ")))
}

/// JSON-shaped JS literals need only key/quote normalization. An expression or
/// spread can override even a literal field, so fall back to source for the
/// whole argument rather than reporting a partially inferred value.
fn literal_arguments(tokens: &[&str]) -> Option<Value> {
    let json = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| {
            if *token == "," && matches!(tokens.get(index + 1), Some(&"}" | &"]")) {
                return None;
            }
            let value = string_literal(token).or_else(|| {
                (tokens.get(index + 1) == Some(&":")
                    && matches!(tokens.get(index.wrapping_sub(1)), Some(&"{" | &",")))
                .then(|| token.to_string())
            });
            Some(value.map_or_else(
                || token.to_string(),
                |value| serde_json::to_string(&value).unwrap(),
            ))
        })
        .collect::<Vec<_>>()
        .join(" ");
    serde_json::from_str(&json).ok()
}

fn string_literal(token: &str) -> Option<String> {
    let quote = token.chars().next()?;
    if !matches!(quote, '\'' | '"' | '`') || token.len() < 2 || !token.ends_with(quote) {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<String>(token) {
        return Some(value);
    }
    let inner = &token[1..token.len() - 1];
    if quote == '`' && inner.contains("${") {
        return None;
    }
    let mut chars = inner.chars();
    let mut value = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            value.push(ch);
            continue;
        }
        match chars.next()? {
            'n' => value.push('\n'),
            'r' => value.push('\r'),
            't' => value.push('\t'),
            '\n' => {}
            '\\' => value.push('\\'),
            escaped if escaped == quote => value.push(escaped),
            // Keep unfamiliar escapes visible instead of guessing their value.
            escaped => {
                value.push('\\');
                value.push(escaped);
            }
        }
    }
    Some(value)
}

/// A lexical scan is enough to find literal tools.foo(...) call sites without
/// mistaking text in strings or comments for calls. This is not a JS runtime.
fn js_tokens(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        match bytes[index] {
            byte if byte.is_ascii_whitespace() => {
                index += 1;
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index < bytes.len() && bytes.get(index..index + 2) != Some(b"*/") {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
                continue;
            }
            b'/' if matches!(
                tokens.last().copied(),
                None | Some(
                    "=" | "("
                        | "["
                        | "{"
                        | ","
                        | ":"
                        | ";"
                        | "!"
                        | "?"
                        | "return"
                        | ">"
                        | "&"
                        | "|"
                )
            ) =>
            {
                index += 1;
                let mut in_class = false;
                while index < bytes.len() && bytes[index] != b'\n' {
                    let byte = bytes[index];
                    index += 1;
                    match byte {
                        b'\\' => index = (index + 1).min(bytes.len()),
                        b'[' => in_class = true,
                        b']' => in_class = false,
                        b'/' if !in_class => break,
                        _ => {}
                    }
                }
            }
            quote @ (b'\'' | b'"' | b'`') => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if byte == b'\\' {
                        index = (index + 1).min(bytes.len());
                    } else if byte == quote {
                        break;
                    }
                }
            }
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') => {
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
                {
                    index += 1;
                }
            }
            _ => index += source[index..].chars().next().unwrap().len_utf8(),
        }
        tokens.push(&source[start..index]);
    }
    tokens
}
