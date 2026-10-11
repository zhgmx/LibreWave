use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

/// One observed baseline, scoped to the daemon process that admitted it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceGeneration {
    epoch: [u8; 16],
    revision: u64,
}

impl DeviceGeneration {
    #[must_use]
    pub const fn new(epoch: [u8; 16], revision: u64) -> Self {
        Self { epoch, revision }
    }
}

impl fmt::Display for DeviceGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.epoch {
            write!(formatter, "{byte:02x}")?;
        }
        write!(formatter, ":{}", self.revision)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceGenerationError;

impl fmt::Display for DeviceGenerationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("device generation must be the epoch:revision token shown by inspection")
    }
}

impl std::error::Error for DeviceGenerationError {}

impl FromStr for DeviceGeneration {
    type Err = DeviceGenerationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (epoch, revision) = value.split_once(':').ok_or(DeviceGenerationError)?;
        if epoch.len() != 32
            || !epoch.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || revision.is_empty()
            || !revision.bytes().all(|b| b.is_ascii_digit())
            || (revision.len() > 1 && revision.starts_with('0'))
        {
            return Err(DeviceGenerationError);
        }
        let mut bytes = [0; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&epoch[index * 2..index * 2 + 2], 16)
                .map_err(|_| DeviceGenerationError)?;
        }
        Ok(Self::new(bytes, revision.parse().map_err(|_| DeviceGenerationError)?))
    }
}

impl Serialize for DeviceGeneration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DeviceGeneration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        token.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_token_round_trips_and_rejects_old_or_ambiguous_forms() {
        let generation = DeviceGeneration::new([0xab; 16], u64::MAX);
        assert_eq!(generation.to_string().parse(), Ok(generation));
        let json = serde_json::to_string(&generation).expect("encode token");
        assert_eq!(serde_json::from_str::<DeviceGeneration>(&json).expect("decode"), generation);
        for invalid in [
            "1",
            "ab:1",
            "00000000000000000000000000000000:01",
            "00000000000000000000000000000000:+1",
            "00000000000000000000000000000000:18446744073709551616",
            "000000000000000000000000000000é:1",
            "ABABABABABABABABABABABABABABABAB:1",
        ] {
            assert!(invalid.parse::<DeviceGeneration>().is_err(), "{invalid}");
        }
        assert!(serde_json::from_str::<DeviceGeneration>("1").is_err());
        assert_ne!(DeviceGeneration::new([1; 16], 1), DeviceGeneration::new([2; 16], 1));
    }
}
