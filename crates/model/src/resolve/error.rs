use crate::{Code, Refusal};
use std::fmt;

/// A resolver refusal. It names the declaring source, the resource and field, the category
/// (never the content) of the rejected value, the conflicting owner when there is one, and the
/// corrective action. Manifest values are never copied into it, so secret-bearing values cannot
/// leak through an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveError(Box<Detail>);

impl std::ops::Deref for ResolveError {
    type Target = Detail;
    fn deref(&self) -> &Detail {
        &self.0
    }
}

/// The fields of a [`ResolveError`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    pub code: Code,
    pub source: String,
    pub resource: Option<String>,
    pub field: Option<String>,
    pub rejected: Option<&'static str>,
    pub conflicts_with: Option<String>,
    pub message: String,
    pub action: String,
}

impl ResolveError {
    pub(crate) fn new(
        code: Code,
        source: impl Into<String>,
        message: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        ResolveError(Box::new(Detail {
            code,
            source: source.into(),
            resource: None,
            field: None,
            rejected: None,
            conflicts_with: None,
            message: message.into(),
            action: action.into(),
        }))
    }

    pub(crate) fn resource(mut self, id: impl Into<String>) -> Self {
        self.0.resource = Some(id.into());
        self
    }

    pub(crate) fn field(mut self, field: impl Into<String>) -> Self {
        self.0.field = Some(field.into());
        self
    }

    pub(crate) fn rejected(mut self, category: &'static str) -> Self {
        self.0.rejected = Some(category);
        self
    }

    pub(crate) fn conflicts_with(mut self, owner: impl Into<String>) -> Self {
        self.0.conflicts_with = Some(owner.into());
        self
    }
}

/// Characters that change how text is laid out or hide in it: zero-width characters, the byte
/// order mark, and the bidirectional embedding, override and isolate controls.
pub(crate) fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
    )
}

/// Replaces control and invisible characters with visible escapes so untrusted text cannot
/// rewrite a terminal or disguise itself.
pub(crate) fn visible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else if is_invisible(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (source: {}",
            visible(&self.message),
            visible(&self.source)
        )?;
        if let Some(r) = &self.resource {
            write!(f, "; resource: {}", visible(r))?;
        }
        if let Some(p) = &self.field {
            write!(f, "; field: {}", visible(p))?;
        }
        if let Some(c) = self.rejected {
            write!(f, "; rejected value: {c}")?;
        }
        if let Some(o) = &self.conflicts_with {
            write!(f, "; conflicts with: {}", visible(o))?;
        }
        write!(f, "). Fix: {}", visible(&self.action))
    }
}

impl std::error::Error for ResolveError {}

impl From<ResolveError> for Refusal {
    fn from(e: ResolveError) -> Refusal {
        Refusal::new(e.code, e.to_string())
    }
}

/// The category of a TOML value, for errors that must not show the value.
pub(crate) fn category(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_invisible_characters_become_visible_escapes() {
        let shown = visible("a\nb\u{202e}c\u{200b}d\u{feff}e\u{2066}f\u{7}");
        assert_eq!(shown, "a\\nb\\u{202e}c\\u{200b}d\\u{feff}e\\u{2066}f\\u{7}");
        assert_eq!(visible("plain ünï text"), "plain ünï text");
    }
}
