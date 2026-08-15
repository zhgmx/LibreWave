use super::hash;
use super::model::{OwnedKind, OwnedPath, Snapshot};
use super::system::UdevSystem;
use super::validation::invalid_data;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) fn prepare_snapshot(path: &Path, saved_content: PathBuf) -> io::Result<Snapshot> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Snapshot {
            destination: path.to_path_buf(),
            kind: Some(OwnedKind::Symlink),
            saved_content: None,
            sha256: None,
            link_target: Some(fs::read_link(path)?),
            mode: None,
        }),
        Ok(metadata) if metadata.is_file() => Ok(Snapshot {
            destination: path.to_path_buf(),
            kind: Some(OwnedKind::File),
            saved_content: Some(saved_content),
            sha256: Some(hash::file(path)?),
            link_target: None,
            mode: Some(metadata.mode() & 0o777),
        }),
        Ok(metadata) if metadata.is_dir() => Ok(Snapshot {
            destination: path.to_path_buf(),
            kind: Some(OwnedKind::Directory),
            saved_content: None,
            sha256: None,
            link_target: None,
            mode: Some(metadata.mode() & 0o777),
        }),
        Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported existing path type")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Snapshot {
            destination: path.to_path_buf(),
            kind: None,
            saved_content: None,
            sha256: None,
            link_target: None,
            mode: None,
        }),
        Err(error) => Err(error),
    }
}

pub(super) fn restore_snapshot(destination: &Path, snapshot: &Snapshot) -> io::Result<()> {
    if destination != snapshot.destination {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "snapshot destination mismatch"));
    }
    if !saved_snapshot_is_valid(snapshot)? {
        return Err(invalid_data("refused to restore a missing or corrupt snapshot"));
    }
    match snapshot.kind {
        None => remove_any(destination),
        Some(OwnedKind::Directory) => {
            if checked_exists(destination)? {
                Ok(())
            } else {
                fs::create_dir_all(destination)
            }
        }
        Some(OwnedKind::Symlink) => {
            remove_any(destination)?;
            ensure_parent(destination)?;
            symlink(
                snapshot.link_target.as_ref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "backup target is missing")
                })?,
                destination,
            )
        }
        Some(OwnedKind::File) => {
            remove_any(destination)?;
            atomic_copy(
                snapshot.saved_content.as_deref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "saved file content is missing")
                })?,
                destination,
                snapshot.mode.unwrap_or(0o644),
            )
        }
    }
}

pub(super) fn restore_udev_snapshot<U: UdevSystem>(
    udev: &mut U,
    destination: &Path,
    snapshot: &Snapshot,
) -> io::Result<()> {
    if destination != snapshot.destination {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "udev snapshot destination mismatch",
        ));
    }
    if !saved_snapshot_is_valid(snapshot)? {
        return Err(invalid_data("refused to restore a missing or corrupt udev snapshot"));
    }
    match snapshot.kind {
        None => udev.remove_and_refresh(destination),
        Some(OwnedKind::File) => udev.install_and_refresh(
            destination,
            &fs::read(snapshot.saved_content.as_deref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "saved udev content is missing")
            })?)?,
        ),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "invalid udev rollback snapshot")),
    }
}

pub(super) fn restore_or_remove(owned: &OwnedPath) -> io::Result<()> {
    match owned.original.as_ref() {
        Some(original) => restore_original(&owned.path, original),
        None if matches!(owned.kind, OwnedKind::Directory) && !owned.created_by_librewave => Ok(()),
        None => remove_entry(&owned.path, &owned.kind),
    }
}

pub(super) fn restore_original(destination: &Path, backup: &Snapshot) -> io::Result<()> {
    if destination != backup.destination {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "original destination mismatch"));
    }
    if !saved_snapshot_is_valid(backup)? {
        return Err(invalid_data("refused to restore a missing or corrupt original backup"));
    }
    remove_any(destination)?;
    match backup.kind {
        Some(OwnedKind::File) => atomic_copy(
            backup.saved_content.as_deref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "saved original content is missing")
            })?,
            destination,
            backup.mode.unwrap_or(0o644),
        ),
        Some(OwnedKind::Symlink) => {
            ensure_parent(destination)?;
            symlink(
                backup.link_target.as_ref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "backup target is missing")
                })?,
                destination,
            )
        }
        Some(OwnedKind::Directory) => fs::create_dir_all(destination),
        None => Ok(()),
    }
}

pub(super) fn matches_entry(owned: &OwnedPath) -> io::Result<bool> {
    match (&owned.kind, fs::symlink_metadata(&owned.path)) {
        (OwnedKind::Directory, Ok(metadata)) => Ok(metadata.is_dir()),
        (OwnedKind::Symlink, Ok(metadata)) if metadata.file_type().is_symlink() => {
            Ok(Some(fs::read_link(&owned.path)?) == owned.link_target)
        }
        (OwnedKind::File, Ok(metadata)) if metadata.is_file() => Ok(owned.sha256.as_deref()
            == Some(hash::file(&owned.path)?.as_str())
            && owned.mode == Some(metadata.mode() & 0o777)),
        (_, Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        (_, Ok(_)) => Ok(false),
        (_, Err(error)) => Err(error),
    }
}

pub(super) fn matches_snapshot_destination(snapshot: &Snapshot) -> io::Result<bool> {
    match snapshot.kind {
        None => Ok(!checked_exists(&snapshot.destination)?),
        Some(OwnedKind::Directory) => match fs::symlink_metadata(&snapshot.destination) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        },
        Some(OwnedKind::Symlink) => match fs::symlink_metadata(&snapshot.destination) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&snapshot.destination)?;
                Ok(snapshot.link_target.as_ref() == Some(&target))
            }
            Ok(_) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        },
        Some(OwnedKind::File) => {
            let metadata = match fs::symlink_metadata(&snapshot.destination) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            Ok(snapshot.sha256.as_deref() == Some(hash::file(&snapshot.destination)?.as_str())
                && snapshot.mode == Some(metadata.mode() & 0o777))
        }
    }
}

pub(super) fn saved_snapshot_is_valid(snapshot: &Snapshot) -> io::Result<bool> {
    let Some(saved) = snapshot.saved_content.as_ref() else {
        return Ok(!matches!(snapshot.kind, Some(OwnedKind::File)));
    };
    let metadata = match fs::symlink_metadata(saved) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(metadata.mode() & 0o777 == 0o600
        && snapshot.sha256.as_deref() == Some(hash::file(saved)?.as_str()))
}

pub(super) fn verify_saved_snapshots(snapshots: &[Snapshot]) -> io::Result<()> {
    for snapshot in snapshots {
        if !saved_snapshot_is_valid(snapshot)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "saved snapshot is missing or corrupt for {}",
                    snapshot.destination.display()
                ),
            ));
        }
    }
    Ok(())
}

pub(super) fn atomic_json<T: serde::Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut content = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    content.push(b'\n');
    atomic_write(path, &content, 0o600)
}

pub(super) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => return Err(invalid_data("lifecycle state is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.mode() & 0o777 != 0o600 {
        return Err(invalid_data("lifecycle state does not have mode 0600"));
    }
    match fs::read(path) {
        Ok(content) => serde_json::from_slice(&content).map(Some).map_err(|error| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{}: {error}", path.display()))
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(invalid_data("lifecycle state disappeared during inspection"))
        }
        Err(error) => Err(error),
    }
}

pub(super) fn atomic_copy(source: &Path, destination: &Path, mode: u32) -> io::Result<()> {
    if source == destination {
        return Ok(());
    }
    let mut input = fs::File::open(source)?;
    let (mut output, temporary) = create_temporary(destination)?;
    let result = (|| {
        io::copy(&mut input, &mut output)?;
        output.set_permissions(fs::Permissions::from_mode(mode))?;
        output.sync_all()?;
        drop(output);
        durable_rename(&temporary, destination)
    })();
    cleanup_temporary_result(result, &temporary)
}

pub(super) fn atomic_copy_new(source: &Path, destination: &Path, mode: u32) -> io::Result<()> {
    let mut input = fs::File::open(source)?;
    let (mut output, temporary) = create_temporary(destination)?;
    let result = (|| {
        io::copy(&mut input, &mut output)?;
        output.set_permissions(fs::Permissions::from_mode(mode))?;
        output.sync_all()?;
        drop(output);
        fs::hard_link(&temporary, destination)?;
        fs::remove_file(&temporary)?;
        if let Some(parent) = destination.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    cleanup_temporary_result(result, &temporary)
}

pub(super) fn atomic_write(path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
    let (mut output, temporary) = create_temporary(path)?;
    let result = (|| {
        output.write_all(content)?;
        output.set_permissions(fs::Permissions::from_mode(mode))?;
        output.sync_all()?;
        drop(output);
        durable_rename(&temporary, path)
    })();
    cleanup_temporary_result(result, &temporary)
}

pub(super) fn atomic_symlink(target: &Path, path: &Path) -> io::Result<()> {
    ensure_parent(path)?;
    for _ in 0..100 {
        let temporary = unique_temporary_path(path);
        match symlink(target, &temporary) {
            Ok(()) => {
                let result = durable_rename(&temporary, path);
                return cleanup_temporary_result(result, &temporary);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "cannot create a unique transaction symlink"))
}

pub(super) fn cleanup_temporary_result(result: io::Result<()>, temporary: &Path) -> io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => match fs::remove_file(temporary) {
            Ok(()) => Err(error),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(error),
            Err(cleanup) => Err(io::Error::other(format!(
                "operation failed: {error}; temporary cleanup also failed: {cleanup}"
            ))),
        },
    }
}

pub(super) fn unique_temporary_path(path: &Path) -> PathBuf {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".librewave-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

pub(super) fn create_temporary(path: &Path) -> io::Result<(fs::File, PathBuf)> {
    ensure_parent(path)?;
    for _ in 0..100 {
        let temporary = unique_temporary_path(path);
        match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary) {
            Ok(file) => return Ok((file, temporary)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "cannot create a unique transaction file"))
}

pub(super) fn durable_rename(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)?;
    if let Some(parent) = destination.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn ensure_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub(super) fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) fn remove_entry(path: &Path, kind: &OwnedKind) -> io::Result<()> {
    match kind {
        OwnedKind::Directory => match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        OwnedKind::File | OwnedKind::Symlink => remove_if_exists(path),
    }
}

pub(super) fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) fn remove_empty(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) fn remove_transaction_files(path: &Path, snapshots: &[Snapshot]) -> io::Result<()> {
    if path.file_name().and_then(|name| name.to_str()) != Some("setup")
        || path.parent().and_then(Path::file_name).and_then(|name| name.to_str())
            != Some("transactions")
    {
        return Err(invalid_data("refused an unexpected transaction cleanup path"));
    }
    for snapshot in snapshots {
        let Some(saved) = snapshot.saved_content.as_ref() else {
            continue;
        };
        if !checked_exists(saved)? {
            continue;
        }
        if !saved_snapshot_is_valid(snapshot)? {
            return Err(invalid_data(format!(
                "refused to remove a modified transaction snapshot: {}",
                saved.display()
            )));
        }
        remove_if_exists(saved)?;
    }
    remove_empty(path)?;
    if let Some(parent) = path.parent() {
        remove_empty(parent)?;
    }
    Ok(())
}

pub(super) fn checked_exists(path: &Path) -> io::Result<bool> {
    classify_existence(fs::symlink_metadata(path))
}

fn classify_existence(result: io::Result<fs::Metadata>) -> io::Result<bool> {
    match result {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existence_checks_propagate_inspection_errors() {
        assert!(
            !classify_existence(Err(io::Error::from(io::ErrorKind::NotFound)))
                .expect("NotFound must be a confirmed absence")
        );
        assert_eq!(
            classify_existence(Err(io::Error::from(io::ErrorKind::PermissionDenied)))
                .expect_err("PermissionDenied must remain an inspection failure")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
