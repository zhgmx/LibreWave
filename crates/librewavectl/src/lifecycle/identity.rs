use super::hash;
use super::model::BuildIdentity;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct IdentityRequest<'a> {
    pub source_root: &'a Path,
    pub profile: &'a str,
    pub target: &'a str,
    pub cli_path: &'a Path,
    pub daemon_path: &'a Path,
}

pub fn discover(request: &IdentityRequest<'_>) -> io::Result<BuildIdentity> {
    let revision = git(request.source_root, &["rev-parse", "HEAD"])?;
    let status = git_bytes(
        request.source_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let source_dirty = !status.is_empty();
    let dirty_fingerprint = source_dirty.then(|| fingerprint(request.source_root)).transpose()?;
    let installed_at_unix_seconds =
        SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_secs();
    Ok(BuildIdentity {
        source_revision: revision.trim().to_owned(),
        source_dirty,
        dirty_fingerprint,
        profile: request.profile.to_owned(),
        target: request.target.to_owned(),
        cli_sha256: hash::file(request.cli_path)?,
        daemon_sha256: hash::file(request.daemon_path)?,
        installed_at_unix_seconds,
    })
}

fn fingerprint(root: &Path) -> io::Result<String> {
    let output = git_bytes(root, &["ls-files", "-co", "--exclude-standard", "-z"])?;
    let mut paths = output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .collect::<Vec<_>>();
    paths.sort();
    let mut fingerprint = hash::Stream::new();
    for relative in paths {
        fingerprint.update(relative.as_os_str().as_encoded_bytes());
        fingerprint.update(&[0]);
        let path = root.join(&relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fingerprint.update(b"link\0");
                fingerprint.update(fs::read_link(path)?.as_os_str().as_encoded_bytes());
            }
            Ok(metadata) if metadata.file_type().is_file() => {
                fingerprint.update(b"file\0");
                fingerprint.update_file(&path)?;
            }
            Ok(metadata) if metadata.file_type().is_dir() => fingerprint.update(b"dir\0"),
            Ok(metadata) if metadata.file_type().is_socket() => {
                fingerprint.update(b"socket\0");
            }
            Ok(_) => fingerprint.update(b"other\0"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fingerprint.update(b"deleted\0");
            }
            Err(error) => return Err(error),
        }
        fingerprint.update(&[0xff]);
    }
    fingerprint.update(&git_bytes(root, &["diff", "--summary", "HEAD"])?);
    Ok(fingerprint.finish())
}

fn git(root: &Path, arguments: &[&str]) -> io::Result<String> {
    String::from_utf8(git_bytes(root, arguments)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn git_bytes(root: &Path, arguments: &[&str]) -> io::Result<Vec<u8>> {
    let output = Command::new("git").args(arguments).current_dir(root).output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

use std::os::unix::ffi::OsStrExt;
