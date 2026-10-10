use crate::Exit;
use std::fmt;

/// Stable error codes. The first group is defined by docs/architecture.md; the
/// second group is added by the bootstrap task and listed in docs/bootstrap.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    HomeMismatch,
    SchemaUnsupported,
    LimitExceeded,
    SourceUntrusted,
    ToolchainMismatch,
    BuildConfig,
    PlatformUnsupported,
    RebindUnsupported,
    BindingInvalid,
    ProfileInvalid,
    IoFailed,
    Usage,
}

impl Code {
    pub const fn as_str(self) -> &'static str {
        match self {
            Code::HomeMismatch => "E_HOME_MISMATCH",
            Code::SchemaUnsupported => "E_SCHEMA_UNSUPPORTED",
            Code::LimitExceeded => "E_LIMIT_EXCEEDED",
            Code::SourceUntrusted => "E_SOURCE_UNTRUSTED",
            Code::ToolchainMismatch => "E_TOOLCHAIN_MISMATCH",
            Code::BuildConfig => "E_BUILD_CONFIG",
            Code::PlatformUnsupported => "E_PLATFORM_UNSUPPORTED",
            Code::RebindUnsupported => "E_REBIND_UNSUPPORTED",
            Code::BindingInvalid => "E_BINDING_INVALID",
            Code::ProfileInvalid => "E_PROFILE_INVALID",
            Code::IoFailed => "E_IO_FAILED",
            Code::Usage => "E_USAGE",
        }
    }

    pub const fn exit(self) -> Exit {
        match self {
            Code::Usage => Exit::Usage,
            // A failure while writing is not a refusal before mutation.
            Code::IoFailed => Exit::Failed,
            _ => Exit::Blocked,
        }
    }
}

/// A refusal before any mutation: a stable code plus the first failing location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: Code,
    pub detail: String,
}

impl Refusal {
    pub fn new(code: Code, detail: impl Into<String>) -> Self {
        Refusal {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for Refusal {}
