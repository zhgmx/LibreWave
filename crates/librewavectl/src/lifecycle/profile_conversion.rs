//! Explicit offline conversion of the local schema-2 development profile.
use super::{engine::InstallPaths, fs_ops, hash, validation};
use librewave_core::{
    MixerGeneration, MixerProfile, MixerSourceSnapshot, OutputControls, SourceId,
};
use serde::{Deserialize, Serialize};
use std::{fs, io, os::unix::fs::MetadataExt, path::Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousProfile {
    schema: u16,
    generation: MixerGeneration,
    microphone_source: SourceId,
    sources: Vec<MixerSourceSnapshot>,
    outputs: OutputControls,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Backup {
    original: Vec<u8>,
    mode: u32,
    converted_sha256: String,
}

fn convert(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let old: PreviousProfile = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    if old.schema != 2 {
        return Err(io::Error::other("profile conversion requires schema 2"));
    }
    let next_source_id = old
        .sources
        .iter()
        .map(|s| s.controls.source().get())
        .max()
        .and_then(|id| id.checked_add(1))
        .ok_or_else(|| io::Error::other("profile has no available source identity"))?;
    let profile = MixerProfile {
        generation: old.generation,
        next_source_id,
        assignments: Vec::new(),
        microphone_source: old.microphone_source,
        sources: old.sources,
        outputs: old.outputs,
    };
    profile.validate().map_err(io::Error::other)?;
    let mut value = serde_json::to_value(profile).map_err(io::Error::other)?;
    value.as_object_mut().expect("profile serializes as object").insert("schema".into(), 3.into());
    serde_json::to_vec(&value).map_err(io::Error::other)
}

fn read_regular(path: &Path) -> io::Result<(Vec<u8>, u32)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!(
            "refused unsafe profile or backup: {}",
            path.display()
        )));
    }
    Ok((fs::read(path)?, metadata.mode() & 0o777))
}

pub(super) fn apply(paths: &InstallPaths, rollback: bool) -> io::Result<()> {
    apply_with(paths, rollback, || Ok(()))
}

fn apply_with(
    paths: &InstallPaths,
    rollback: bool,
    before_write: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    validation::validate_lifecycle_paths(paths)?;
    let profile = paths.profiles().join("mixer-state.json");
    let pending = paths.profiles().join(".mixer-state.json.pending");
    let backup_path = paths.state_root.join("mixer-schema-2-backup.json");
    for path in [&profile, &backup_path] {
        validation::validate_parent_chain(path, paths)?;
    }
    match fs::symlink_metadata(&pending) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
        Ok(_) => return Err(io::Error::other("pending mixer write exists; conversion refused")),
    }
    let (current, current_mode) = read_regular(&profile)?;
    let backup = match read_regular(&backup_path) {
        Ok((bytes, _)) => serde_json::from_slice::<Backup>(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !rollback => {
            let converted = convert(&current)?;
            let backup = Backup {
                original: current.clone(),
                mode: current_mode,
                converted_sha256: hash::bytes(&converted),
            };
            fs_ops::atomic_write_absent(
                &backup_path,
                &serde_json::to_vec(&backup).map_err(io::Error::other)?,
                0o600,
            )?;
            backup
        }
        Err(error) => return Err(error),
    };
    let converted = convert(&backup.original)?;
    if hash::bytes(&converted) != backup.converted_sha256 || backup.mode & !0o755 != 0 {
        return Err(io::Error::other("profile conversion backup is inconsistent"));
    }
    let target = if rollback { &backup.original } else { &converted };
    if current == *target && current_mode == backup.mode {
        return Ok(());
    }
    let expected = if rollback { &converted } else { &backup.original };
    if current != *expected || current_mode != backup.mode {
        return Err(io::Error::other(
            "profile changed since conversion backup; preserve it before retrying",
        ));
    }
    before_write()?;
    if read_regular(&profile)? != (current, current_mode) {
        return Err(io::Error::other("profile changed during conversion"));
    }
    fs_ops::atomic_write(&profile, target, backup.mode)?;
    if read_regular(&profile)? != (target.clone(), backup.mode) {
        return Err(io::Error::other("profile conversion verification failed; backup retained"));
    }
    Ok(())
}

pub(super) fn run<
    U: super::system::UdevSystem,
    S: super::system::UnitInspector,
    P: super::system::ProcessInspector,
>(
    arguments: &[String],
    lifecycle: &super::engine::Lifecycle<U, S, P>,
    output: &mut impl std::io::Write,
) -> io::Result<i32> {
    let rollback = match &arguments[2..] {
        [] => false,
        [flag] if flag == "--rollback" => true,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "use convert-profile [--rollback]",
            ));
        }
    };
    let _lock = super::runtime::lock(&lifecycle.paths)?;
    lifecycle.preflight_runtime()?;
    apply(&lifecycle.paths, rollback)?;
    writeln!(
        output,
        "Mixer profile {}. Exact backup retained at {}",
        if rollback { "restored" } else { "converted to schema 3" },
        lifecycle.paths.state_root.join("mixer-schema-2-backup.json").display()
    )?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{engine::test_paths, tests::Fixture};
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn prepare(fixture: &Fixture) -> (InstallPaths, Vec<u8>) {
        let paths = test_paths(&fixture.root);
        let profile = MixerProfile::default()
            .with_output(
                librewave_core::MixTarget::Stream,
                librewave_core::MixRoute::new(
                    false,
                    librewave_core::FaderGain::from_decibels(-30.0).expect("level"),
                ),
            )
            .expect("output");
        let mut value = serde_json::to_value(profile).expect("profile");
        let object = value.as_object_mut().expect("object");
        object.remove("next_source_id");
        object.remove("assignments");
        object.insert("schema".into(), 2.into());
        object.insert("generation".into(), 122.into());
        let bytes = serde_json::to_vec_pretty(&value).expect("serialize");
        let path = paths.profiles().join("mixer-state.json");
        fs::create_dir_all(paths.profiles()).expect("profile directory");
        fs::write(&path, &bytes).expect("fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("mode");
        (paths, bytes)
    }

    #[test]
    fn conversion_preserves_existing_fields_and_rollback_restores_exact_bytes_and_mode() {
        let fixture = Fixture::new();
        let (paths, original) = prepare(&fixture);
        apply(&paths, false).expect("convert");
        apply(&paths, false).expect("repeat conversion");
        let path = paths.profiles().join("mixer-state.json");
        let (converted, mode) = read_regular(&path).expect("read");
        assert_eq!(mode, 0o640);
        let mut current: serde_json::Value = serde_json::from_slice(&converted).expect("current");
        assert_eq!(current["next_source_id"], 3);
        assert_eq!(current["assignments"], serde_json::json!([]));
        current.as_object_mut().expect("object").remove("next_source_id");
        current.as_object_mut().expect("object").remove("assignments");
        current["schema"] = 2.into();
        assert_eq!(current, serde_json::from_slice::<serde_json::Value>(&original).expect("old"));
        apply(&paths, true).expect("rollback");
        apply(&paths, true).expect("repeat rollback");
        assert_eq!(read_regular(&path).expect("read"), (original, 0o640));
        assert!(paths.state_root.join("mixer-schema-2-backup.json").is_file());
    }

    #[test]
    fn interruption_after_backup_can_resume_or_restore_without_losing_original() {
        let fixture = Fixture::new();
        let (paths, original) = prepare(&fixture);
        assert!(
            apply_with(&paths, false, || Err(io::Error::other("injected write failure"))).is_err()
        );
        assert_eq!(fs::read(paths.profiles().join("mixer-state.json")).expect("read"), original);
        apply(&paths, true).expect("original already intact");
        apply(&paths, false).expect("resume");
        apply(&paths, true).expect("restore");
        assert_eq!(fs::read(paths.profiles().join("mixer-state.json")).expect("read"), original);
    }

    #[test]
    fn changed_profiles_pending_writes_and_symlinks_are_preserved() {
        let fixture = Fixture::new();
        let (paths, _) = prepare(&fixture);
        let path = paths.profiles().join("mixer-state.json");
        let pending = paths.profiles().join(".mixer-state.json.pending");
        fs::write(&pending, b"pending").expect("pending");
        assert!(apply(&paths, false).is_err());
        fs::remove_file(&pending).expect("remove fixture pending");
        apply(&paths, false).expect("convert");
        fs::write(&path, b"user edit").expect("edit");
        assert!(apply(&paths, true).is_err());
        assert_eq!(fs::read(&path).expect("read"), b"user edit");
        fs::remove_file(&path).expect("remove fixture file");
        symlink(&pending, &path).expect("symlink");
        assert!(apply(&paths, false).is_err());
        assert!(fs::symlink_metadata(&path).expect("metadata").file_type().is_symlink());
    }

    #[test]
    fn failed_rollback_and_concurrent_edit_preserve_recoverable_states() {
        let fixture = Fixture::new();
        let (paths, original) = prepare(&fixture);
        apply(&paths, false).expect("convert");
        let path = paths.profiles().join("mixer-state.json");
        let converted = fs::read(&path).expect("converted");
        assert!(
            apply_with(&paths, true, || Err(io::Error::other("injected rollback failure")))
                .is_err()
        );
        assert_eq!(fs::read(&path).expect("read"), converted);
        apply(&paths, true).expect("retry rollback");
        assert_eq!(fs::read(&path).expect("read"), original);
        assert!(apply_with(&paths, false, || fs::write(&path, b"concurrent edit")).is_err());
        assert_eq!(fs::read(&path).expect("read"), b"concurrent edit");
    }

    #[test]
    fn command_refuses_an_active_daemon_before_writing_backup() {
        let fixture = Fixture::new();
        let (paths, original) = prepare(&fixture);
        let mut lifecycle = fixture.lifecycle();
        lifecycle.unit_inspector.active = true;
        let arguments = vec!["librewavectl".to_owned(), "convert-profile".to_owned()];
        assert!(run(&arguments, &lifecycle, &mut Vec::new()).is_err());
        assert_eq!(fs::read(paths.profiles().join("mixer-state.json")).expect("read"), original);
        assert!(!paths.state_root.join("mixer-schema-2-backup.json").exists());
    }

    #[test]
    fn malformed_or_unknown_profiles_create_no_backup() {
        let fixture = Fixture::new();
        let (paths, _) = prepare(&fixture);
        fs::write(paths.profiles().join("mixer-state.json"), b"{}").expect("invalid fixture");
        assert!(apply(&paths, false).is_err());
        assert!(!paths.state_root.join("mixer-schema-2-backup.json").exists());
        assert!(convert(&serde_json::to_vec(&MixerProfile::default()).expect("profile")).is_err());
    }
}
