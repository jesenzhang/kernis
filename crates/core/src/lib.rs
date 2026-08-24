//! Neutral primitives shared only when their semantics are genuinely identical.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Stable textual identifier used across experiments.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Id(String);

impl Id {
    /// Creates a non-empty identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidId`] when the value is empty or only whitespace.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(InvalidId);
        }
        Ok(Self(value))
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Error returned when an identifier is empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidId;

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("identifier must not be empty")
    }
}

impl std::error::Error for InvalidId {}

/// Monotonic revision number for versioned structures.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Revision(u64);

impl Revision {
    /// Initial revision.
    pub const ZERO: Self = Self(0);

    /// Largest representable revision.
    pub const MAX: Self = Self(u64::MAX);

    /// Returns the raw revision value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next revision.
    #[must_use]
    pub const fn next(self) -> Self {
        self.checked_next().expect("revision overflow")
    }

    /// Returns the next revision, or None when the counter is exhausted.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_blank_ids() {
        assert_eq!(Id::new("   "), Err(InvalidId));
    }

    #[test]
    fn revision_is_monotonic() {
        assert!(Revision::ZERO.next() > Revision::ZERO);
    }

    #[test]
    fn revision_overflow_is_not_silent() {
        assert_eq!(Revision::MAX.checked_next(), None);
        assert!(std::panic::catch_unwind(|| Revision::MAX.next()).is_err());
    }
}
