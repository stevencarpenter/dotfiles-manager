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

pub(crate) use crate::display::{is_invisible, visible};

impl ResolveError {
    /// The one-line text, with `esc` applied to every field that can hold text from a source.
    fn text(&self, esc: fn(&str) -> String) -> String {
        let mut out = format!("{} (source: {}", esc(&self.message), esc(&self.source));
        if let Some(r) = &self.resource {
            out.push_str(&format!("; resource: {}", esc(r)));
        }
        if let Some(p) = &self.field {
            out.push_str(&format!("; field: {}", esc(p)));
        }
        if let Some(c) = self.rejected {
            out.push_str(&format!("; rejected value: {c}"));
        }
        if let Some(o) = &self.conflicts_with {
            out.push_str(&format!("; conflicts with: {}", esc(o)));
        }
        out.push_str(&format!("). Fix: {}", esc(&self.action)));
        out
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text(visible))
    }
}

impl std::error::Error for ResolveError {}

/// The detail keeps the raw text: the command that prints a refusal escapes it once.
impl From<ResolveError> for Refusal {
    fn from(e: ResolveError) -> Refusal {
        Refusal::new(e.code, e.text(str::to_string))
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
