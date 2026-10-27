//! A tiny JSON tree and pretty-printer (no dependencies).

/// A JSON value for the `--json` report.
#[derive(Debug, Clone)]
pub enum Json {
    Null,
    Bool(bool),
    Number(u64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn object(fields: impl IntoIterator<Item = (impl Into<String>, Json)>) -> Self {
        Self::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn array(items: impl IntoIterator<Item = Json>) -> Self {
        Self::Array(items.into_iter().collect())
    }

    pub fn string(value: impl Into<String>) -> Self {
        Self::String(value.into())
    }

    /// Serialize with two-space indentation and a trailing newline.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        self.write(true, 0, &mut out);
        out.push('\n');
        out
    }

    fn write(&self, pretty: bool, indent: usize, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => out.push_str(&value.to_string()),
            Self::String(value) => out.push_str(&quote(value)),
            Self::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    newline(pretty, indent + 1, out);
                    item.write(pretty, indent + 1, out);
                }
                newline(pretty, indent, out);
                out.push(']');
            }
            Self::Object(fields) => {
                if fields.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    newline(pretty, indent + 1, out);
                    out.push_str(&quote(key));
                    out.push_str(": ");
                    value.write(pretty, indent + 1, out);
                }
                newline(pretty, indent, out);
                out.push('}');
            }
        }
    }
}

fn newline(pretty: bool, indent: usize, out: &mut String) {
    if pretty {
        out.push('\n');
        for _ in 0..indent {
            out.push_str("  ");
        }
    }
}

fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_strings() {
        assert_eq!(quote("a\"b\\c\n"), r#""a\"b\\c\n""#);
        assert_eq!(quote("\u{1}"), r#""\u0001""#);
    }

    #[test]
    fn pretty_shape() {
        let value = Json::object([
            ("a", Json::Number(1)),
            ("b", Json::array([Json::string("x"), Json::Null])),
        ]);
        let text = value.pretty();
        assert!(text.contains("\"a\": 1"), "{text}");
        assert!(text.contains("\"x\""), "{text}");
        assert!(text.ends_with("}\n"), "{text}");
    }
}
