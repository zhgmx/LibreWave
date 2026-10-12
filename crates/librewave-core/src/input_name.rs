//! Bounded user-facing software input names.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A nonempty UTF-8 input name with at most 64 bytes and no control characters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputName {
    bytes: [u8; 64],
    length: u8,
}

impl InputName {
    /// Validates an input name before it enters a profile or platform property.
    /// # Errors
    /// Returns an error for an empty, overlong, or control-character name.
    pub fn try_new(value: &str) -> Result<Self, InputNameError> {
        if value.is_empty()
            || value.len() > 64
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err(InputNameError);
        }
        let mut bytes = [0; 64];
        bytes[..value.len()].copy_from_slice(value.as_bytes());
        Ok(Self { bytes, length: u8::try_from(value.len()).map_err(|_| InputNameError)? })
    }

    #[must_use]
    /// Returns the validated UTF-8 name.
    /// # Panics
    /// Panics if the private UTF-8 invariant is violated. Public constructors preserve it.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.length)])
            .expect("InputName stores validated UTF-8")
    }
}

impl Serialize for InputName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for InputName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::try_new(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputNameError;
impl fmt::Display for InputNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("input names need 1 to 64 UTF-8 bytes without surrounding whitespace or control characters")
    }
}
impl std::error::Error for InputNameError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_bounded_and_cannot_inject_platform_lines() {
        for value in ["", " Music", "Music ", "Music\n", "x\0y"] {
            assert!(InputName::try_new(value).is_err());
        }
        assert!(InputName::try_new(&"x".repeat(65)).is_err());
        for value in ["Music", "Voice chat", "日本語", "quote \" ; $(literal)"] {
            let name = InputName::try_new(value).expect("valid name");
            assert_eq!(name.as_str(), value);
            assert_eq!(
                serde_json::from_str::<InputName>(&serde_json::to_string(&name).expect("encode"))
                    .expect("decode"),
                name
            );
        }
    }
}
