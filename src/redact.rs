use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

static PATTERNS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    [
        (
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
            "[redacted]",
        ),
        (r"\bgh[oprsu]_[A-Za-z0-9]{20,}", "[redacted]"),
        (r"\bgithub_pat_[A-Za-z0-9_]{20,}", "[redacted]"),
        (r"\bsk-[A-Za-z0-9_-]{16,}", "[redacted]"),
        (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "[redacted]"),
        (
            r"(?P<label>\b(?i:bearer)\s+)[A-Za-z0-9._~+/-]{16,}=*",
            "${label}[redacted]",
        ),
        (
            r#"(?P<label>\b[A-Za-z0-9_]*(?i:key|token|secret|password)\s*=\s*)(?:"[^"\n]*"|'[^'\n]*'|[^\s"']+)"#,
            "${label}[redacted]",
        ),
    ]
    .into_iter()
    .map(|(pattern, replacement)| {
        (
            Regex::new(pattern).expect("secret pattern compiles"),
            replacement,
        )
    })
    .collect()
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redacted(String);

impl Redacted {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn cut(mut self, chars: usize) -> Redacted {
        if let Some((end, _)) = self.0.char_indices().nth(chars) {
            self.0.truncate(end);
        }
        self
    }
}

/// Replaces credentials that commonly leak into transcripts with `[redacted]`.
pub fn secrets(text: &str) -> Redacted {
    Redacted(replace(text))
}

/// Formats a tool call as `<name> <input JSON>`.
pub fn tool_call(name: &str, input: &Value) -> Redacted {
    // Redacting each string before serializing keeps JSON escapes from hiding a secret's quotes or newlines.
    let mut input = input.clone();
    redact_strings(&mut input);
    Redacted(format!("{} {input}", replace(name)))
}

fn replace(text: &str) -> String {
    let mut text = text.to_owned();
    for (pattern, replacement) in PATTERNS.iter() {
        if let Cow::Owned(replaced) = pattern.replace_all(&text, *replacement) {
            text = replaced;
        }
    }
    text
}

fn redact_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = replace(text),
        Value::Array(items) => items.iter_mut().for_each(redact_strings),
        Value::Object(fields) => fields.values_mut().for_each(redact_strings),
        _ => {}
    }
}
