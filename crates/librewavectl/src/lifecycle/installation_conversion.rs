//! Explicit conversion of the schema-4 installation record without changing its artifacts.
use super::{
    engine::{InstallPaths, Lifecycle},
    fs_ops,
    model::Manifest,
    system::{ProcessInspector, UdevSystem, UnitInspector},
    validation,
};
use std::{fs, io, os::unix::fs::MetadataExt};

fn convert(bytes: &[u8], paths: &InstallPaths) -> io::Result<Manifest> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    if value.get("schema_version") != Some(&4.into())
        || value.get("desktop").is_some()
        || value.get("identity").and_then(|v| v.get("gui_sha256")).is_some()
    {
        return Err(io::Error::other(
            "installation conversion requires an unchanged schema-4 record",
        ));
    }
    value["schema_version"] = 5.into();
    value["desktop"] = serde_json::Value::Null;
    let manifest: Manifest = serde_json::from_value(value).map_err(io::Error::other)?;
    validation::validate_manifest(&manifest, paths)?;
    Ok(manifest)
}

pub(super) fn run<U: UdevSystem, S: UnitInspector, P: ProcessInspector>(
    arguments: &[String],
    lifecycle: &Lifecycle<U, S, P>,
    output: &mut impl io::Write,
) -> io::Result<i32> {
    if arguments.len() != 2 {
        return Err(io::Error::other("use convert-installation without options"));
    }
    let paths = &lifecycle.paths;
    let _lock = super::runtime::lock(paths)?;
    lifecycle.preflight_runtime()?;
    validation::validate_lifecycle_paths(paths)?;
    if fs_ops::checked_exists(&paths.journal())? {
        return Err(io::Error::other(
            "recover the existing installation transaction before conversion",
        ));
    }
    let path = paths.manifest();
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::other("unsafe installation manifest"));
    }
    let original = fs::read(&path)?;
    let manifest = convert(&original, paths)?;
    for owned in &manifest.owned_paths {
        if !lifecycle.matches_owned(owned)? {
            return Err(io::Error::other(format!(
                "installation artifact changed: {}",
                owned.path.display()
            )));
        }
    }
    let backup = paths.state_root.join("install-manifest-schema-4.json");
    match fs::symlink_metadata(&backup) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs_ops::atomic_write_absent(&backup, &original, metadata.mode() & 0o777)?;
        }
        Err(error) => return Err(error),
        Ok(meta) if meta.is_file() && fs::read(&backup)? == original => {}
        Ok(_) => {
            return Err(io::Error::other(
                "installation conversion backup differs; preserve it before retrying",
            ));
        }
    }
    if fs::read(&path)? != original {
        return Err(io::Error::other("installation record changed during conversion"));
    }
    fs_ops::atomic_write(
        &path,
        &serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?,
        metadata.mode() & 0o777,
    )?;
    lifecycle.read_manifest()?;
    writeln!(
        output,
        "Installation record converted. Artifacts and USB permissions are unchanged. Original record: {}",
        backup.display()
    )?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::tests::Fixture;

    #[test]
    fn conversion_retains_build_key_owned_paths_and_usb_baseline() {
        let fixture = Fixture::new();
        let mut lifecycle = fixture.lifecycle();
        let current = fixture.install(&mut lifecycle, "schema-four");
        let mut old = serde_json::to_value(&current).expect("manifest");
        old["schema_version"] = 4.into();
        old.as_object_mut().expect("object").remove("desktop");
        let bytes = serde_json::to_vec(&old).expect("old manifest");
        assert_eq!(convert(&bytes, &lifecycle.paths).expect("convert"), current);
        fs::write(lifecycle.paths.manifest(), &bytes).expect("fixture record");
        run(&["librewavectl".into(), "convert-installation".into()], &lifecycle, &mut Vec::new())
            .expect("convert record");
        assert_eq!(lifecycle.read_manifest().expect("read"), Some(current));
        assert_eq!(
            fs::read(lifecycle.paths.state_root.join("install-manifest-schema-4.json"))
                .expect("backup"),
            bytes
        );
    }

    #[test]
    fn conversion_rejects_unknown_schema_and_modified_artifacts_without_backup() {
        let fixture = Fixture::new();
        let mut lifecycle = fixture.lifecycle();
        let current = fixture.install(&mut lifecycle, "schema-four");
        assert!(
            convert(&serde_json::to_vec(&current).expect("manifest"), &lifecycle.paths).is_err()
        );
        let mut old = serde_json::to_value(&current).expect("manifest");
        old["schema_version"] = 4.into();
        old.as_object_mut().expect("object").remove("desktop");
        fs::write(lifecycle.paths.manifest(), serde_json::to_vec(&old).expect("old"))
            .expect("record");
        fs::write(current.active_build_directory.join("bin/librewaved"), b"modified")
            .expect("modified");
        assert!(
            run(
                &["librewavectl".into(), "convert-installation".into()],
                &lifecycle,
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(!lifecycle.paths.state_root.join("install-manifest-schema-4.json").exists());
    }
}
