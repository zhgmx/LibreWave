use super::artifacts::render_unit;
use super::engine::InstallPaths;
use super::hash;
use super::model::{
    Journal, MANIFEST_SCHEMA, Manifest, Operation, OwnedKind, OwnedPath, Phase, Snapshot,
};
use librewave_platform_linux::audio_policy::Wave3AudioPolicy;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path};

pub(super) fn validate_manifest(manifest: &Manifest, paths: &InstallPaths) -> io::Result<()> {
    validate_lifecycle_paths(paths)?;
    validate_schema(manifest.schema_version, "manifest")?;
    validate_absolute(&manifest.active_build_directory)?;
    let expected_build = paths.installations().join(manifest.identity.stable_key());
    if manifest.active_build_directory != expected_build
        || manifest.unit_path != paths.unit()
        || manifest.profiles_path != paths.profiles()
    {
        return Err(invalid_data("manifest contains an unexpected build, unit, or profile path"));
    }
    let allowed = [&paths.data_root, &paths.state_root, &paths.config_root, &paths.bin_root];
    let mut unique = BTreeSet::new();
    let expected_paths = BTreeMap::from([
        (paths.data_root.clone(), OwnedKind::Directory),
        (paths.installations(), OwnedKind::Directory),
        (expected_build.clone(), OwnedKind::Directory),
        (expected_build.join("bin"), OwnedKind::Directory),
        (expected_build.join("bin/librewavectl"), OwnedKind::File),
        (expected_build.join("bin/librewaved"), OwnedKind::File),
        (paths.unit(), OwnedKind::File),
        (paths.udev_rule.clone(), OwnedKind::File),
        (paths.active_link(), OwnedKind::Symlink),
        (paths.bin_root.join("librewavectl"), OwnedKind::Symlink),
        (paths.bin_root.join("librewaved"), OwnedKind::Symlink),
    ]);
    if manifest.identity.source_dirty != manifest.identity.dirty_fingerprint.is_some()
        || manifest.identity.dirty_fingerprint.as_deref().is_some_and(|value| !valid_hash(value))
        || !valid_hash(&manifest.identity.cli_sha256)
        || !valid_hash(&manifest.identity.daemon_sha256)
    {
        return Err(invalid_data("manifest build identity is inconsistent"));
    }
    if manifest.owned_paths.len() != expected_paths.len() {
        return Err(invalid_data("manifest owned-path set is not exact"));
    }
    for (index, entry) in manifest.owned_paths.iter().enumerate() {
        validate_absolute(&entry.path)?;
        if !unique.insert(entry.path.clone()) {
            return Err(invalid_data("manifest contains a duplicate owned path"));
        }
        if entry.path != paths.udev_rule && !allowed.iter().any(|root| entry.path.starts_with(root))
        {
            return Err(invalid_data(format!(
                "manifest path is outside LibreWave roots: {}",
                entry.path.display()
            )));
        }
        if expected_paths.get(&entry.path) != Some(&entry.kind) {
            return Err(invalid_data(format!(
                "manifest contains an unexpected owned path or kind: {}",
                entry.path.display()
            )));
        }
        validate_exact_metadata(entry, manifest, paths, &expected_build)?;
        validate_owned(entry, paths)?;
        if let Some(saved) =
            entry.original.as_ref().and_then(|original| original.saved_content.as_ref())
            && saved != &paths.backup_root().join(format!("original-{index}"))
        {
            return Err(invalid_data("original backup path is not exact"));
        }
        validate_parent_chain(&entry.path, paths)?;
    }
    if manifest.udev_policy_path != paths.udev_rule
        || manifest.wireplumber_policy_path != paths.wireplumber()
        || manifest.active_link != paths.active_link()
    {
        return Err(invalid_data("manifest contains an unexpected fixed path"));
    }
    Ok(())
}

pub(super) fn validate_lifecycle_paths(paths: &InstallPaths) -> io::Result<()> {
    for path in [
        &paths.data_root,
        &paths.state_root,
        &paths.config_root,
        &paths.bin_root,
        &paths.udev_rule,
        &paths.manifest(),
        &paths.journal(),
        &paths.backup_root(),
        &paths.state_root.join("transactions/setup"),
    ] {
        validate_absolute(path)?;
        validate_parent_chain(path, paths)?;
    }
    Ok(())
}

fn validate_exact_metadata(
    entry: &OwnedPath,
    manifest: &Manifest,
    paths: &InstallPaths,
    expected_build: &Path,
) -> io::Result<()> {
    if entry.path == expected_build.join("bin/librewavectl") {
        if entry.sha256.as_deref() != Some(manifest.identity.cli_sha256.as_str())
            || entry.mode != Some(0o755)
        {
            return Err(invalid_data("manifest CLI metadata is not exact"));
        }
    } else if entry.path == expected_build.join("bin/librewaved") {
        if entry.sha256.as_deref() != Some(manifest.identity.daemon_sha256.as_str())
            || entry.mode != Some(0o755)
        {
            return Err(invalid_data("manifest daemon metadata is not exact"));
        }
    } else if entry.path == paths.unit() {
        let unit_hash =
            hash::bytes(render_unit(&paths.active_link().join("bin/librewaved")).as_bytes());
        if entry.sha256.as_deref() != Some(unit_hash.as_str()) || entry.mode != Some(0o644) {
            return Err(invalid_data("manifest unit metadata is not exact"));
        }
    } else if entry.path == paths.udev_rule {
        let udev_hash = hash::bytes(Wave3AudioPolicy::new().render_udev().as_bytes());
        if entry.sha256.as_deref() != Some(udev_hash.as_str()) || entry.mode != Some(0o644) {
            return Err(invalid_data("manifest udev metadata is not exact"));
        }
    } else if entry.path == paths.active_link() {
        if entry.link_target.as_deref() != Some(expected_build) {
            return Err(invalid_data("manifest active link target is not exact"));
        }
    } else if entry.path == paths.bin_root.join("librewavectl") {
        if entry.link_target != Some(paths.active_link().join("bin/librewavectl")) {
            return Err(invalid_data("manifest CLI link target is not exact"));
        }
    } else if entry.path == paths.bin_root.join("librewaved")
        && entry.link_target != Some(paths.active_link().join("bin/librewaved"))
    {
        return Err(invalid_data("manifest daemon link target is not exact"));
    }
    Ok(())
}

pub(super) fn validate_journal(journal: &Journal, paths: &InstallPaths) -> io::Result<()> {
    validate_schema(journal.schema_version, "journal")?;
    if let Some(previous) = journal.previous.as_ref() {
        validate_manifest(previous, paths)?;
    }
    if let Some(next) = journal.next.as_ref() {
        validate_manifest(next, paths)?;
    }
    match journal.operation {
        Operation::Setup => validate_setup_journal(journal, paths),
        Operation::Uninstall => {
            if journal.previous.is_none()
                || journal.next.is_some()
                || !journal.rollback_paths.is_empty()
                || journal.udev_refresh_pending && journal.phase != Phase::Prepared
                || !matches!(journal.phase, Phase::Prepared | Phase::Applied | Phase::Committed)
            {
                return Err(invalid_data("uninstall journal contains setup state"));
            }
            Ok(())
        }
    }
}

fn validate_setup_journal(journal: &Journal, paths: &InstallPaths) -> io::Result<()> {
    if journal.purge_profiles
        || journal.udev_refresh_pending
        || !matches!(
            journal.phase,
            Phase::Prepared
                | Phase::BackedUp
                | Phase::Applied
                | Phase::ManifestSwitched
                | Phase::Committed
        )
    {
        return Err(invalid_data("setup journal contains a profile purge request"));
    }
    let next =
        journal.next.as_ref().ok_or_else(|| invalid_data("setup journal has no next manifest"))?;
    if journal.rollback_paths.len() != next.owned_paths.len() {
        return Err(invalid_data("setup journal rollback count does not match the manifest"));
    }
    for (index, (snapshot, owned)) in
        journal.rollback_paths.iter().zip(&next.owned_paths).enumerate()
    {
        if snapshot.destination != owned.path {
            return Err(invalid_data("journal rollback destination does not match its owned path"));
        }
        validate_snapshot(snapshot, paths, true)?;
        if owned.path == paths.udev_rule {
            match snapshot.kind {
                None => {}
                Some(OwnedKind::File)
                    if snapshot.sha256.as_deref()
                        == Some(
                            hash::bytes(Wave3AudioPolicy::new().render_udev().as_bytes()).as_str(),
                        )
                        && snapshot.mode == Some(0o644) => {}
                _ => {
                    return Err(invalid_data(
                        "the udev rollback snapshot is not absent or the exact mode-0644 rule",
                    ));
                }
            }
        }
        if (matches!(owned.kind, OwnedKind::Directory)
            && !matches!(snapshot.kind, None | Some(OwnedKind::Directory)))
            || (!matches!(owned.kind, OwnedKind::Directory)
                && matches!(snapshot.kind, Some(OwnedKind::Directory)))
        {
            return Err(invalid_data(
                "journal snapshot kind does not match its destination contract",
            ));
        }
        if let Some(saved) = snapshot.saved_content.as_ref()
            && saved != &paths.state_root.join(format!("transactions/setup/{index}.backup"))
        {
            return Err(invalid_data("journal backup path is not exact"));
        }
    }
    Ok(())
}

fn validate_owned(entry: &OwnedPath, paths: &InstallPaths) -> io::Result<()> {
    match entry.kind {
        OwnedKind::File => {
            if !entry.sha256.as_deref().is_some_and(valid_hash)
                || !matches!(entry.mode, Some(0o644 | 0o755))
                || entry.link_target.is_some()
                || entry.created_by_librewave
            {
                return Err(invalid_data("manifest file metadata is inconsistent"));
            }
        }
        OwnedKind::Symlink => {
            if entry.sha256.is_some()
                || entry.mode.is_some()
                || entry.link_target.as_ref().is_none_or(|target| !target.is_absolute())
                || entry.created_by_librewave
            {
                return Err(invalid_data("manifest symlink metadata is inconsistent"));
            }
            validate_absolute(entry.link_target.as_deref().expect("checked above"))?;
        }
        OwnedKind::Directory => {
            if entry.sha256.is_some()
                || entry.mode.is_some()
                || entry.link_target.is_some()
                || entry.original.is_some()
            {
                return Err(invalid_data("manifest directory metadata is inconsistent"));
            }
        }
    }
    if entry.path == paths.udev_rule && entry.original.is_some() {
        return Err(invalid_data("the fixed udev rule cannot replace foreign content"));
    }
    if let Some(original) = entry.original.as_ref() {
        if original.destination != entry.path {
            return Err(invalid_data(
                "original snapshot destination does not match its owned path",
            ));
        }
        validate_snapshot(original, paths, false)?;
        if matches!(original.kind, Some(OwnedKind::Directory)) {
            return Err(invalid_data("a file or symlink cannot replace an existing directory"));
        }
    }
    Ok(())
}

fn validate_snapshot(
    snapshot: &Snapshot,
    paths: &InstallPaths,
    transaction: bool,
) -> io::Result<()> {
    validate_absolute(&snapshot.destination)?;
    if let Some(saved) = snapshot.saved_content.as_ref() {
        validate_absolute(saved)?;
        validate_parent_chain(saved, paths)?;
    }
    let destination_allowed = snapshot.destination == paths.udev_rule
        || [&paths.data_root, &paths.state_root, &paths.config_root, &paths.bin_root]
            .iter()
            .any(|root| snapshot.destination.starts_with(root));
    if !destination_allowed {
        return Err(invalid_data("snapshot destination is outside LibreWave roots"));
    }
    match snapshot.kind {
        None => {
            if snapshot.saved_content.is_some()
                || snapshot.sha256.is_some()
                || snapshot.link_target.is_some()
                || snapshot.mode.is_some()
            {
                return Err(invalid_data("absent snapshot contains file metadata"));
            }
        }
        Some(OwnedKind::File) => {
            let saved = snapshot
                .saved_content
                .as_ref()
                .ok_or_else(|| invalid_data("file snapshot has no saved content"))?;
            let expected_root = if transaction {
                paths.state_root.join("transactions/setup")
            } else {
                paths.backup_root()
            };
            if !saved.starts_with(expected_root)
                || !snapshot.sha256.as_deref().is_some_and(valid_hash)
                || snapshot.mode.is_none()
                || snapshot.link_target.is_some()
            {
                return Err(invalid_data("file snapshot metadata is inconsistent"));
            }
        }
        Some(OwnedKind::Symlink) => {
            if snapshot.saved_content.is_some()
                || snapshot.sha256.is_some()
                || snapshot.mode.is_some()
                || snapshot.link_target.is_none()
            {
                return Err(invalid_data("symlink snapshot metadata is inconsistent"));
            }
        }
        Some(OwnedKind::Directory) => {
            if snapshot.saved_content.is_some()
                || snapshot.sha256.is_some()
                || snapshot.link_target.is_some()
            {
                return Err(invalid_data("directory snapshot metadata is inconsistent"));
            }
        }
    }
    Ok(())
}

fn validate_parent_chain(path: &Path, _paths: &InstallPaths) -> io::Result<()> {
    validate_absolute(path)?;
    let mut parent = std::path::PathBuf::from("/");
    let components = path.components().collect::<Vec<_>>();
    for component in components.iter().skip(1).take(components.len().saturating_sub(2)) {
        parent.push(component.as_os_str());
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid_data(format!(
                    "manifest parent is a symlink: {}",
                    parent.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn validate_schema(schema: u32, name: &str) -> io::Result<()> {
    if schema == MANIFEST_SCHEMA {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "unsupported {name} schema {schema}; remove this pre-release installation explicitly"
        )))
    }
}

fn validate_absolute(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.components().any(|part| matches!(part, Component::ParentDir)) {
        Err(invalid_data("manifest path is not absolute and normalized"))
    } else {
        Ok(())
    }
}

pub(super) fn validate_purge_path(path: &Path, config_root: &Path) -> io::Result<()> {
    if path == config_root.join("librewave/profiles") {
        Ok(())
    } else {
        Err(invalid_data("refused an unexpected profile purge path"))
    }
}

pub(super) fn validate_systemd_path(path: &Path) -> io::Result<()> {
    if path.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the installation path must be valid UTF-8 for systemd ExecStart",
        ));
    }
    let value = path.as_os_str().as_encoded_bytes();
    if value.iter().any(|byte| {
        byte.is_ascii_whitespace()
            || byte.is_ascii_control()
            || matches!(byte, b'"' | b'\\' | b'%' | b'$')
    }) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the installation path cannot be represented safely in systemd ExecStart",
        ))
    } else {
        Ok(())
    }
}
