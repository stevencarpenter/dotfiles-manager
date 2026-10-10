use crate::Exit;
use std::fmt;

/// Stable error codes. The first group is defined by docs/architecture.md; the
/// second group is added by the bootstrap task (docs/bootstrap.md) and the third by the
/// state task (docs/state.md).
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
    StateInvalid,
    StateNewer,
    StorageInvalid,
    IdentityMismatch,
    Locked,
    OpInterrupted,
    Diagnosed,
    IoUnconfirmed,
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
            Code::StateInvalid => "E_STATE_INVALID",
            Code::StateNewer => "E_STATE_NEWER",
            Code::StorageInvalid => "E_STORAGE_INVALID",
            Code::IdentityMismatch => "E_IDENTITY_MISMATCH",
            Code::Locked => "E_LOCKED",
            Code::OpInterrupted => "E_OP_INTERRUPTED",
            Code::Diagnosed => "E_DIAGNOSED",
            Code::IoUnconfirmed => "E_IO_UNCONFIRMED",
        }
    }

    pub const fn exit(self) -> Exit {
        match self {
            Code::Usage => Exit::Usage,
            // A failure while writing, or findings from a read-only check, are not refusals.
            Code::IoFailed | Code::Diagnosed | Code::IoUnconfirmed => Exit::Failed,
            _ => Exit::Blocked,
        }
    }
}

/// A refusal before any mutation: a stable code plus the first failing location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: Code,
    pub detail: String,
    /// True when state was already changed before this refusal. Section 10: a gate that fails
    /// after a mutation began exits 1, not 3.
    pub mutated: bool,
}

impl Refusal {
    pub fn new(code: Code, detail: impl Into<String>) -> Self {
        Refusal {
            code,
            detail: detail.into(),
            mutated: false,
        }
    }

    /// Marks that a mutation had begun when this refusal happened.
    pub fn after_mutation(mut self) -> Self {
        self.mutated = true;
        self
    }

    /// The process exit for this refusal.
    pub fn exit(&self) -> Exit {
        match self.code.exit() {
            Exit::Blocked if self.mutated => Exit::Failed,
            e => e,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for Refusal {}
