//! Stable typed module references.
//!
//! A [`ModuleReference`] is the loader-layer declarative handle for one
//! logical module at one exact [`ModuleVersion`]. It is the host's
//! configuration language: the host names which modules it wants, and the
//! loader resolves each exact reference against an explicit catalog.
//!
//! Version identity is exact by contract — there is no range, no solver, and
//! no implicit fallback. `ModuleVersion` is loader/catalog resolution
//! metadata; the durable execution identity remains the K2 `RunDefinition`
//! canonical identity underneath the composition.
//!
//! # Textual grammar
//!
//! The display and serde form of a reference is `<module-id>@<module-version>
//! `, and the *final* `@` is the delimiter. A module [`Id`] may contain `@`;
//! a [`ModuleVersion`] may not. The grammar is therefore unambiguous and the
//! textual representation is bijective: `org@app@1` always parses back to id
//! `org@app` with version `1`, and
//! `deserialize(serialize(reference)) == reference` holds for every
//! reference. A version containing `@` is rejected at typed construction
//! with [`InvalidReferenceReason::ReservedVersionDelimiter`](crate::InvalidReferenceReason)
//! — never silently reinterpreted.

use crate::error::{InvalidReferenceReason, LoaderError};
use kernis_core::Id;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Typed, non-empty exact module version value.
///
/// K5 treats the version as an opaque stable label compared by exact
/// equality, not as a Solver-compatible semantic version. Reordering,
/// parsing, or comparing version content across releases is deliberately
/// not attempted: the same version string is the same version, and any
/// other value is a different version that must resolve to a different
/// catalog entry or fail closed.
///
/// The textual `ModuleReference` grammar reserves the final `@` as the
/// id/version delimiter, so a version must not contain `@`. Module ids may.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModuleVersion(String);

impl ModuleVersion {
    /// Creates a version from a non-empty, non-whitespace value that does
    /// not contain the reserved `@` reference delimiter.
    ///
    /// # Errors
    ///
    /// Returns [`LoaderError::InvalidReference`] when the value is blank or
    /// contains `@`.
    pub fn new(value: impl Into<String>) -> Result<Self, LoaderError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(LoaderError::InvalidReference {
                value,
                reason: InvalidReferenceReason::BlankVersion,
            });
        }
        if value.contains('@') {
            return Err(LoaderError::InvalidReference {
                value,
                reason: InvalidReferenceReason::ReservedVersionDelimiter,
            });
        }
        Ok(Self(value))
    }

    /// Returns the version as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModuleVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ModuleVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ModuleVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Stable logical reference to one module at one exact version.
///
/// The reference is the key of the loader domain: catalog entries are
/// registered and looked up by exact reference, dependency edges name exact
/// references, and the total order over references is the deterministic
/// tie-breaker for every loader operation, so resolution cannot depend on
/// catalog insertion order or root input order.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModuleReference {
    id: Id,
    version: ModuleVersion,
}

impl ModuleReference {
    /// Creates a reference from a module id and version string.
    ///
    /// The module id may contain `@`; the version may not (the textual
    /// grammar uses the *final* `@` as the id/version delimiter).
    ///
    /// # Errors
    ///
    /// Returns [`LoaderError::InvalidReference`] when either part is blank
    /// or the version contains the reserved `@` delimiter.
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> Result<Self, LoaderError> {
        let id_value = id.into();
        let parsed_id = Id::new(id_value.clone()).map_err(|_| LoaderError::InvalidReference {
            value: id_value,
            reason: InvalidReferenceReason::BlankModuleId,
        })?;
        Ok(Self {
            id: parsed_id,
            version: ModuleVersion::new(version)?,
        })
    }

    /// Returns the logical module identity.
    #[must_use]
    pub fn id(&self) -> &Id {
        &self.id
    }

    /// Returns the exact version identity.
    #[must_use]
    pub fn version(&self) -> &ModuleVersion {
        &self.version
    }
}

impl fmt::Display for ModuleReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.id, self.version)
    }
}

impl Serialize for ModuleReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ModuleReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let (id, version) = value.rsplit_once('@').ok_or_else(|| {
            serde::de::Error::custom("module reference must have the form id@version")
        })?;
        Self::new(id, version).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{InvalidReferenceReason, LoaderError, ModuleReference, ModuleVersion};

    #[test]
    fn rejects_blank_versions() {
        assert!(ModuleVersion::new("  ").is_err());
    }

    #[test]
    fn rejects_blank_reference_parts() {
        assert!(ModuleReference::new(" ", "1").is_err());
        assert!(ModuleReference::new("app", " ").is_err());
    }

    #[test]
    fn rejects_the_reserved_delimiter_in_versions() {
        assert_eq!(
            ModuleVersion::new("v@1"),
            Err(LoaderError::InvalidReference {
                value: "v@1".to_owned(),
                reason: InvalidReferenceReason::ReservedVersionDelimiter,
            })
        );
        assert_eq!(
            ModuleReference::new("app", "v@1"),
            Err(LoaderError::InvalidReference {
                value: "v@1".to_owned(),
                reason: InvalidReferenceReason::ReservedVersionDelimiter,
            })
        );
    }

    #[test]
    fn ids_may_contain_the_delimiter() {
        let reference = ModuleReference::new("org@app", "1").expect("ids may contain '@'");
        assert_eq!(reference.to_string(), "org@app@1");
        let encoded = serde_json::to_string(&reference).expect("serializes");
        assert_eq!(encoded, "\"org@app@1\"");
        let decoded: ModuleReference = serde_json::from_str(&encoded).expect("deserializes");
        assert_eq!(decoded, reference);
        assert_eq!(decoded.id().as_str(), "org@app");
        assert_eq!(decoded.version().as_str(), "1");
    }

    #[test]
    fn textual_reference_round_trips_are_bijective() {
        for (id, version) in [
            ("app", "1"),
            ("org@app", "1"),
            ("complex-module", "2026.09"),
            ("a@b@c", "2"),
            ("scoped/pkg", "v1.0-beta+build.7"),
        ] {
            let reference =
                ModuleReference::new(id, version).expect("every listed pair is a valid reference");
            let encoded = serde_json::to_string(&reference).expect("serializes");
            let decoded: ModuleReference = serde_json::from_str(&encoded).expect("deserializes");
            assert_eq!(
                decoded, reference,
                "deserialize(serialize({id}@{version})) must return the original reference"
            );
            assert_eq!(encoded, format!("\"{id}@{version}\""));
        }
    }

    #[test]
    fn invalid_external_textual_references_fail_closed() {
        for rejected in ["app@", "@1", "no-version"] {
            assert!(
                serde_json::from_str::<ModuleReference>(&format!("\"{rejected}\"")).is_err(),
                "\"{rejected}\" is not a valid textual module reference"
            );
        }

        // The grammar is the final '@': "app@v@1" is id "app@v" with
        // version "1" — never id "app" with version "v@1" — and it
        // round-trips as itself.
        let decoded: ModuleReference =
            serde_json::from_str("\"app@v@1\"").expect("final '@' is the delimiter");
        assert_eq!(decoded.id().as_str(), "app@v");
        assert_eq!(decoded.version().as_str(), "1");
        assert_eq!(decoded, ModuleReference::new("app@v", "1").expect("valid"));
        let encoded = serde_json::to_string(&decoded).expect("serializes");
        assert_eq!(encoded, "\"app@v@1\"");
    }

    #[test]
    fn reference_displays_and_orders_stably() {
        let a1 = ModuleReference::new("app", "1").expect("valid");
        let a2 = ModuleReference::new("app", "2").expect("valid");
        let b1 = ModuleReference::new("beta", "1").expect("valid");
        assert_eq!(a1.to_string(), "app@1");
        assert!(a1 < a2, "version breaks ties within one module id");
        assert!(a2 < b1, "module id dominates the total order");
    }

    #[test]
    fn serde_round_trips_through_the_display_form() {
        let reference = ModuleReference::new("app", "1").expect("valid");
        let encoded = serde_json::to_string(&reference).expect("serializes");
        assert_eq!(encoded, "\"app@1\"");
        let decoded: ModuleReference = serde_json::from_str(&encoded).expect("deserializes");
        assert_eq!(decoded, reference);
        assert!(
            serde_json::from_str::<ModuleReference>("\"noversion\"").is_err(),
            "a reference without an exact version fails closed"
        );
    }
}
