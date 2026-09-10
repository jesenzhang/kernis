//! Typed loader failures, kept distinct from composition and activation errors.

use crate::reference::ModuleReference;
use kernis_core::Id;
use std::error::Error;
use std::fmt;

/// Failure reported by a [`ModuleRegistrationFactory`](crate::ModuleRegistrationFactory)
/// while constructing one fresh process-local registration.
///
/// The factory builds plain process-local Rust values only. It runs during
/// loader resolution and therefore never performs a Runtime-owned effect:
/// nothing here activates a Runtime, starts a fiber, dispatches an effect,
/// writes the durable store, or runs a lifecycle hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleFactoryError {
    reason: String,
}

impl ModuleFactoryError {
    /// Creates a factory failure with a host-provided reason.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Returns the factory-provided failure reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for ModuleFactoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl Error for ModuleFactoryError {}

/// Which typed part of a reference string failed validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidReferenceReason {
    /// The module identity was empty or whitespace.
    BlankModuleId,
    /// The module version was empty or whitespace.
    BlankVersion,
    /// The module version contained `@`, which the textual reference
    /// grammar reserves as the final-delimiter between the module id and
    /// the version.
    ReservedVersionDelimiter,
}

impl fmt::Display for InvalidReferenceReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankModuleId => write!(f, "module id must not be empty"),
            Self::BlankVersion => write!(f, "module version must not be empty"),
            Self::ReservedVersionDelimiter => {
                write!(
                    f,
                    "'@' is reserved as the ModuleReference version delimiter"
                )
            }
        }
    }
}

/// Why a resolved catalog entry disagrees with the registration its own
/// factory produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IncompatibleEntryReason {
    /// The produced `ModuleDefinition` carries a different module identity
    /// than the catalog reference under which it was resolved.
    ProducedIdentityMismatch {
        /// The module identity the produced definition actually carries.
        actual: Id,
    },
    /// The produced `ModuleDefinition`'s module dependencies disagree, at
    /// logical dependency identity, with the reference dependencies the
    /// catalog entry declared.
    DeclaredDependenciesDisagree {
        /// Logical module ids declared by the catalog entry, sorted.
        declared: Vec<Id>,
        /// Logical module ids carried by the produced definition, sorted.
        produced: Vec<Id>,
    },
}

impl fmt::Display for IncompatibleEntryReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProducedIdentityMismatch { actual } => write!(
                f,
                "the produced module definition carries identity {actual}, which differs from \
                 the catalog reference"
            ),
            Self::DeclaredDependenciesDisagree { declared, produced } => write!(
                f,
                "the catalog declares dependencies {declared:?} but the produced module \
                 definition depends on {produced:?}"
            ),
        }
    }
}

/// Typed failure of reference resolution against a module catalog.
///
/// Loader failures never masquerade as composition or activation failures:
/// [`CompositionError`](runtime_composition::CompositionError) and
/// [`StartupFailure`](runtime_composition::StartupFailure) stay in their own
/// phases, so a host always learns which layer rejected its request. Every
/// variant names the requested module identity and version it could not
/// satisfy, and resolution failures produce no Runtime-owned effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoaderError {
    /// A module id or version string was rejected at typed construction.
    InvalidReference {
        /// The rejected raw value.
        value: String,
        /// Which part of the reference was invalid.
        reason: InvalidReferenceReason,
    },
    /// The catalog already contains an entry for this exact reference.
    /// Registration never silently overwrites.
    DuplicateCatalogEntry {
        /// The reference registered twice.
        reference: ModuleReference,
    },
    /// The same reference was requested as a root more than once.
    /// Resolution fails closed instead of silently deduplicating host
    /// configuration.
    DuplicateRootReference {
        /// The reference requested twice as a root.
        reference: ModuleReference,
    },
    /// A requested reference (a root, or a declared reference dependency)
    /// has no catalog entry. Exact-version lookup never substitutes a
    /// different version of the same module identity.
    MissingReference {
        /// The reference that could not be found.
        requested: ModuleReference,
        /// The entry that declared this reference as a dependency, or `None`
        /// when the reference was itself requested as a root.
        required_by: Option<ModuleReference>,
        /// Deterministic reference path from the requesting root through the
        /// dependency edges down to `requested` (inclusive).
        path: Vec<ModuleReference>,
    },
    /// The reference dependency graph reachable from the roots contains a
    /// cycle. The loader classifies it here so it never surfaces later as a
    /// composition dependency cycle.
    DependencyCycle {
        /// Deterministic closed cycle path, rotated to start at the
        /// smallest reference and repeated at the end.
        path: Vec<ModuleReference>,
    },
    /// A resolved entry's own factory produced a registration that
    /// contradicts the catalog entry: a different module identity, or
    /// module dependencies that disagree with the declared reference
    /// dependencies at logical identity.
    IncompatibleEntry {
        /// The catalog reference whose produced registration contradicted it.
        reference: ModuleReference,
        /// Structured disagreement classification.
        reason: IncompatibleEntryReason,
    },
    /// A resolved entry's registration factory reported a failure while
    /// constructing the fresh process-local registration.
    RegistrationConstructionFailed {
        /// The catalog reference whose factory failed.
        reference: ModuleReference,
        /// The factory-provided failure.
        source: ModuleFactoryError,
    },
}

impl fmt::Display for LoaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReference { value, reason } => {
                write!(f, "invalid module reference {value:?}: {reason}")
            }
            Self::DuplicateCatalogEntry { reference } => {
                write!(f, "catalog entry {reference} is registered twice")
            }
            Self::DuplicateRootReference { reference } => {
                write!(f, "root reference {reference} is requested twice")
            }
            Self::MissingReference {
                requested,
                required_by,
                path,
            } => {
                write!(f, "reference {requested} is not in the catalog")?;
                match required_by {
                    Some(required_by) => write!(f, ", required by {required_by}")?,
                    None => write!(f, ", requested as a root")?,
                }
                write!(f, "; reference path: {}", format_path(path))
            }
            Self::DependencyCycle { path } => {
                write!(f, "reference dependency cycle: {}", format_path(path))
            }
            Self::IncompatibleEntry { reference, reason } => {
                write!(f, "catalog entry {reference} is incompatible: {reason}")
            }
            Self::RegistrationConstructionFailed { reference, source } => write!(
                f,
                "registration construction failed for reference {reference}: {source}"
            ),
        }
    }
}

impl Error for LoaderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RegistrationConstructionFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn format_path(path: &[ModuleReference]) -> String {
    path.iter()
        .map(ModuleReference::to_string)
        .collect::<Vec<_>>()
        .join(" -> ")
}
