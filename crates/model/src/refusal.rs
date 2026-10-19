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
    /// The requested report cannot be produced because a source cannot be read.
    ReportUnavailable,
    // Resolver codes. The `expected_error` values in examples/cases.toml and E_ADAPTER_UNKNOWN
    // (docs/architecture.md section 11) are fixed by the docs. The others are added by DFM-15
    // and listed in docs/resolver.md.
    OverrideForbidden,
    OverrideConflict,
    CoreReplacement,
    ModuleUnknown,
    ModuleDuplicate,
    DependencyCycle,
    BundleCycle,
    ExtensionNamespace,
    ExtensionConflict,
    SourceMissing,
    SourceEscape,
    TargetCollision,
    TargetInvalid,
    AdapterUnknown,
    FieldUnknown,
    ManifestInvalid,
    ReferenceInvalid,
    TypeMismatch,
    // Planner codes added by DFM-16 (docs/planner.md). E_CONFLICT is named by docs/architecture.md
    // section 8; the others are new.
    Conflict,
    ParentUnsafe,
    PlanCycle,
    PlanStale,
    PlanInvalid,
    PrivilegeUnsupported,
    ObservationUnknown,
    TargetReserved,
    // Apply codes added by DFM-18 (docs/apply.md).
    GeneratorUnavailable,
    DisruptiveUnapproved,
    SensitiveUnsupported,
    ApprovalRequired,
    // Added by DFM-45 (docs/apply.md, adoption).
    AdoptUnapproved,
    // Added by DFM-47 (docs/apply.md, signals). The stop was requested by SIGINT or SIGTERM.
    Interrupted,
    Terminated,
    // Added by DFM-55 (docs/architecture.md section 10, docs/resolver.md). The bound profile
    // targets another platform or architecture than this machine.
    PlatformMismatch,
    // Added by DFM-22 (docs/secrets.md). The vault credential is not available to a
    // noninteractive run, so secret rendering stops before any mutation.
    CredentialUnavailable,
    // Added by DFM-22 (docs/secrets.md). The provider ran and produced no usable output: the
    // previous file is kept.
    RenderFailed,
    // Added by DFM-23 (docs/packages.md). A package probe could not decide the installed state.
    ProbeFailed,
    // Added by DFM-26 (docs/lifecycle.md). A launcher accepted a request and the completion was
    // not observed within the bound: the operation is `pending`, exit 5, never a success.
    Pending,
}

impl Code {
    /// Every code, for parsing a stored plan.
    pub const ALL: &'static [Code] = &[
        Code::HomeMismatch,
        Code::SchemaUnsupported,
        Code::LimitExceeded,
        Code::SourceUntrusted,
        Code::ToolchainMismatch,
        Code::BuildConfig,
        Code::PlatformUnsupported,
        Code::RebindUnsupported,
        Code::BindingInvalid,
        Code::ProfileInvalid,
        Code::IoFailed,
        Code::Usage,
        Code::StateInvalid,
        Code::StateNewer,
        Code::StorageInvalid,
        Code::IdentityMismatch,
        Code::Locked,
        Code::OpInterrupted,
        Code::Diagnosed,
        Code::IoUnconfirmed,
        Code::ReportUnavailable,
        Code::OverrideForbidden,
        Code::OverrideConflict,
        Code::CoreReplacement,
        Code::ModuleUnknown,
        Code::ModuleDuplicate,
        Code::DependencyCycle,
        Code::BundleCycle,
        Code::ExtensionNamespace,
        Code::ExtensionConflict,
        Code::SourceMissing,
        Code::SourceEscape,
        Code::TargetCollision,
        Code::TargetInvalid,
        Code::AdapterUnknown,
        Code::FieldUnknown,
        Code::ManifestInvalid,
        Code::ReferenceInvalid,
        Code::TypeMismatch,
        Code::Conflict,
        Code::ParentUnsafe,
        Code::PlanCycle,
        Code::PlanStale,
        Code::PlanInvalid,
        Code::PrivilegeUnsupported,
        Code::ObservationUnknown,
        Code::TargetReserved,
        Code::GeneratorUnavailable,
        Code::DisruptiveUnapproved,
        Code::SensitiveUnsupported,
        Code::ApprovalRequired,
        Code::AdoptUnapproved,
        Code::Interrupted,
        Code::Terminated,
        Code::PlatformMismatch,
        Code::CredentialUnavailable,
        Code::RenderFailed,
        Code::ProbeFailed,
        Code::Pending,
    ];

    pub fn parse(s: &str) -> Option<Code> {
        Code::ALL.iter().copied().find(|c| c.as_str() == s)
    }

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
            Code::ReportUnavailable => "E_REPORT_UNAVAILABLE",
            Code::OverrideForbidden => "E_OVERRIDE_FORBIDDEN",
            Code::OverrideConflict => "E_OVERRIDE_CONFLICT",
            Code::CoreReplacement => "E_CORE_REPLACEMENT",
            Code::ModuleUnknown => "E_MODULE_UNKNOWN",
            Code::ModuleDuplicate => "E_MODULE_DUPLICATE",
            Code::DependencyCycle => "E_DEPENDENCY_CYCLE",
            Code::BundleCycle => "E_BUNDLE_CYCLE",
            Code::ExtensionNamespace => "E_EXTENSION_NAMESPACE",
            Code::ExtensionConflict => "E_EXTENSION_CONFLICT",
            Code::SourceMissing => "E_SOURCE_MISSING",
            Code::SourceEscape => "E_SOURCE_ESCAPE",
            Code::TargetCollision => "E_TARGET_COLLISION",
            Code::TargetInvalid => "E_TARGET_INVALID",
            Code::AdapterUnknown => "E_ADAPTER_UNKNOWN",
            Code::FieldUnknown => "E_FIELD_UNKNOWN",
            Code::ManifestInvalid => "E_MANIFEST_INVALID",
            Code::ReferenceInvalid => "E_REFERENCE_INVALID",
            Code::TypeMismatch => "E_TYPE_MISMATCH",
            Code::Conflict => "E_CONFLICT",
            Code::ParentUnsafe => "E_PARENT_UNSAFE",
            Code::PlanCycle => "E_PLAN_CYCLE",
            Code::PlanStale => "E_PLAN_STALE",
            Code::PlanInvalid => "E_PLAN_INVALID",
            Code::PrivilegeUnsupported => "E_PRIVILEGE_UNSUPPORTED",
            Code::ObservationUnknown => "E_OBSERVATION_UNKNOWN",
            Code::TargetReserved => "E_TARGET_RESERVED",
            Code::GeneratorUnavailable => "E_GENERATOR_UNAVAILABLE",
            Code::DisruptiveUnapproved => "E_DISRUPTIVE_UNAPPROVED",
            Code::SensitiveUnsupported => "E_SENSITIVE_UNSUPPORTED",
            Code::ApprovalRequired => "E_APPROVAL_REQUIRED",
            Code::AdoptUnapproved => "E_ADOPT_UNAPPROVED",
            Code::Interrupted => "E_INTERRUPTED",
            Code::Terminated => "E_TERMINATED",
            Code::PlatformMismatch => "E_PLATFORM_MISMATCH",
            Code::CredentialUnavailable => "E_CREDENTIAL_UNAVAILABLE",
            Code::RenderFailed => "E_RENDER_FAILED",
            Code::ProbeFailed => "E_PROBE_FAILED",
            Code::Pending => "E_PENDING",
        }
    }

    pub const fn exit(self) -> Exit {
        match self {
            Code::Usage => Exit::Usage,
            // docs/architecture.md section 10, exit 2: "inability to produce the requested report".
            Code::ReportUnavailable => Exit::Usage,
            // A failure while writing, or findings from a read-only check, are not refusals.
            Code::IoFailed | Code::Diagnosed | Code::IoUnconfirmed => Exit::Failed,
            // A provider that ran and failed is a failed operation, as an I/O failure is.
            Code::RenderFailed => Exit::Failed,
            // Section 10: a stop by signal exits 130 or 143, also after a mutation.
            Code::Interrupted => Exit::Interrupted,
            Code::Terminated => Exit::Terminated,
            // Section 10: a queued request whose completion was not observed exits 5.
            Code::Pending => Exit::Pending,
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

#[cfg(test)]
mod tests {
    use super::Code;

    #[test]
    fn every_code_round_trips_through_its_name() {
        for c in Code::ALL {
            assert_eq!(Code::parse(c.as_str()), Some(*c));
        }
        assert_eq!(Code::parse("E_NOPE"), None);
        let names: std::collections::BTreeSet<_> = Code::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(names.len(), Code::ALL.len(), "two codes share a name");
    }
}
