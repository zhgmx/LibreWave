use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MANIFEST_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentity {
    pub source_revision: String,
    pub source_dirty: bool,
    pub dirty_fingerprint: Option<String>,
    pub profile: String,
    pub target: String,
    pub cli_sha256: String,
    pub daemon_sha256: String,
    pub installed_at_unix_seconds: u64,
}

impl BuildIdentity {
    pub fn stable_key(&self) -> String {
        let mut value = self.clone();
        value.installed_at_unix_seconds = 0;
        super::hash::bytes(&serde_json::to_vec(&value).expect("build identity serializes"))[..16]
            .to_owned()
    }

    pub fn same_build(&self, other: &Self) -> bool {
        let mut left = self.clone();
        let mut right = other.clone();
        left.installed_at_unix_seconds = 0;
        right.installed_at_unix_seconds = 0;
        left == right
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnedKind {
    File,
    Symlink,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub destination: PathBuf,
    pub kind: Option<OwnedKind>,
    pub saved_content: Option<PathBuf>,
    pub sha256: Option<String>,
    pub link_target: Option<PathBuf>,
    pub mode: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedPath {
    pub path: PathBuf,
    pub kind: OwnedKind,
    pub sha256: Option<String>,
    pub link_target: Option<PathBuf>,
    pub mode: Option<u32>,
    pub created_by_librewave: bool,
    pub original: Option<Snapshot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub identity: BuildIdentity,
    pub active_build_directory: PathBuf,
    pub active_link: PathBuf,
    pub unit_path: PathBuf,
    pub udev_policy_path: PathBuf,
    pub wireplumber_policy_path: PathBuf,
    pub profiles_path: PathBuf,
    pub audio_ownership: AudioOwnership,
    pub owned_paths: Vec<OwnedPath>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioOwnership {
    BlockedMissingProductionHost,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Setup,
    Uninstall,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    BackedUp,
    Applied,
    ManifestSwitched,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub schema_version: u32,
    pub operation: Operation,
    pub phase: Phase,
    pub previous: Option<Manifest>,
    pub next: Option<Manifest>,
    pub rollback_paths: Vec<Snapshot>,
    pub udev_refresh_pending: bool,
    pub purge_profiles: bool,
}
