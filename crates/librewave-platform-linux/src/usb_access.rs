//! Serial-free installation ownership of a connected Wave:3 node's ACL.
//! Permission inspection uses sysfs and filesystem metadata, never USB requests.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SYS_USB: &str = "/sys/bus/usb/devices";
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// An exact node incarnation and its pre-installation permissions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessSnapshot {
    pub node: PathBuf,
    pub topology: String,
    pub boot_id: String,
    pub filesystem: u64,
    pub inode: u64,
    pub device: u64,
    pub owner: u32,
    pub group: u32,
    pub grant_uid: u32,
    pub original_acl: String,
}

/// Required permission state at an installation boundary.
#[derive(Clone, Copy)]
pub enum AccessState {
    Granted,
    Restored,
    Restorable,
}

/// Validate a permission record before using it in a privileged operation.
/// # Errors
/// Rejects arbitrary paths, users, ACL fields, and duplicate nodes.
pub fn validate_snapshots(snapshots: &[AccessSnapshot]) -> io::Result<()> {
    if snapshots.len() > 32 {
        return Err(invalid("too many Wave:3 access records"));
    }
    if serde_json::to_vec(snapshots).map_err(io::Error::other)?.len() > 65_536 {
        return Err(invalid("access restoration input is too large"));
    }
    let mut nodes = BTreeSet::new();
    for saved in snapshots {
        if saved.topology.is_empty()
            || saved.topology.len() > 64
            || saved.topology.starts_with('.')
            || !saved.topology.bytes().all(|c| c.is_ascii_digit() || c == b'-' || c == b'.')
            || saved.boot_id.len() != 36
            || !saved.boot_id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-')
            || saved.grant_uid == 0
            || !nodes.insert(saved.node.clone())
        {
            return Err(invalid("invalid or duplicate Wave:3 access identity"));
        }
        let parts: Vec<_> = saved.node.components().collect();
        let valid_node = saved.node.to_str().is_some_and(|node| {
            node.strip_prefix("/dev/bus/usb/").is_some_and(|suffix| {
                suffix.len() == 7
                    && suffix.as_bytes()[3] == b'/'
                    && suffix.bytes().enumerate().all(|(i, c)| i == 3 || c.is_ascii_digit())
            })
        });
        if !valid_node || parts.len() != 6 {
            return Err(invalid("access record is outside an exact USB device node"));
        }
        granted_acl(&saved.original_acl, saved.grant_uid)?;
    }
    Ok(())
}

/// Read connected normal-mode Wave:3 ACLs before the access rule is installed.
/// # Errors
/// Rejects inaccessible, ambiguous, or independently managed permissions.
pub fn snapshot_access() -> io::Result<Vec<AccessSnapshot>> {
    // SAFETY: getuid reads the calling user's identity and has no pointer arguments.
    let grant_uid = unsafe { libc::getuid() };
    let boot_id = fs::read_to_string(BOOT_ID)?.trim().to_owned();
    let mut result = Vec::new();
    for (topology, node) in connected_nodes()? {
        let file = open_node(&node)?;
        let stat = file.metadata()?;
        result.push(AccessSnapshot {
            node,
            topology,
            boot_id: boot_id.clone(),
            filesystem: stat.dev(),
            inode: stat.ino(),
            device: stat.rdev(),
            owner: stat.uid(),
            group: stat.gid(),
            grant_uid,
            original_acl: read_acl(&descriptor_path(&file))?,
        });
    }
    validate_snapshots(&result)?;
    Ok(result)
}

/// Check that current connected nodes have an owned, unchanged ACL baseline.
/// An unknown incarnation is preserved and blocks removal rather than being guessed.
/// # Errors
/// Returns missing ownership, identity, or independently changed ACL errors.
pub fn verify_access(snapshots: &[AccessSnapshot], state: AccessState) -> io::Result<()> {
    for (saved, file) in owned_nodes(snapshots)? {
        let original = canonical_acl(&saved.original_acl)?;
        let current = read_acl(&descriptor_path(&file))?;
        let granted = granted_acl(&original, saved.grant_uid)?;
        let matches = match state {
            AccessState::Granted => current == granted,
            AccessState::Restored => current == original,
            AccessState::Restorable => current == original || current == granted,
        };
        if !matches {
            return Err(invalid(format!(
                "USB ACL differs from its owned state at {}; preserve the access records",
                saved.node.display()
            )));
        }
    }
    Ok(())
}

/// Restore original ACLs in the exact privileged installer helper.
/// Already restored records are idempotent. Foreign changes are preserved.
/// # Errors
/// Rejects non-root callers, forged users, unowned incarnations, or changed ACLs.
pub fn restore_access(snapshots: &[AccessSnapshot]) -> io::Result<()> {
    // SAFETY: geteuid reads process credentials and has no pointer arguments.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ACL restoration requires the exact privileged installer helper",
        ));
    }
    let caller = std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|uid| uid.parse::<u32>().ok())
        .ok_or_else(|| invalid("the privileged access helper has no invoking user"))?;
    if snapshots.iter().any(|saved| saved.grant_uid != caller) {
        return Err(invalid("access records belong to another invoking user"));
    }
    if Path::new("/etc/udev/rules.d/70-librewave-wave3.rules").try_exists()? {
        return Err(invalid("remove the owned access rule before restoring USB ACLs"));
    }
    let nodes = owned_nodes(snapshots)?;
    // Validate every live ACL before changing any node.
    let mut changes = Vec::new();
    for (saved, file) in nodes {
        let original = canonical_acl(&saved.original_acl)?;
        let current = read_acl(&descriptor_path(&file))?;
        if current == original {
            continue;
        }
        if current != granted_acl(&original, saved.grant_uid)? {
            return Err(invalid(format!(
                "USB permissions changed independently at {}; preserved",
                saved.node.display()
            )));
        }
        changes.push((file, original, current));
    }
    for (file, original, expected) in changes {
        let path = descriptor_path(&file);
        if read_acl(&path)? != expected {
            return Err(invalid("USB permissions changed before restoration; preserved"));
        }
        set_acl(&path, &original)?;
        if read_acl(&path)? != original {
            return Err(invalid("original USB ACL restoration was not verified"));
        }
    }
    verify_access(snapshots, AccessState::Restored)
}

/// Run the bounded privileged helper input. No USB control API is reachable here.
/// # Errors
/// Returns decoding, credential, identity, ACL, or output failures.
pub fn restore_access_input(input: &mut impl Read) -> io::Result<()> {
    let mut bytes = Vec::new();
    input.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        return Err(invalid("access restoration input is too large"));
    }
    let records: Vec<AccessSnapshot> = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    restore_access(&records)
}

fn owned_nodes(snapshots: &[AccessSnapshot]) -> io::Result<Vec<(&AccessSnapshot, File)>> {
    validate_snapshots(snapshots)?;
    let boot = fs::read_to_string(BOOT_ID)?;
    let mut result = Vec::new();
    for (topology, node) in connected_nodes()? {
        let file = open_node(&node)?;
        let stat = file.metadata()?;
        let saved = snapshots.iter().find(|saved| saved.topology == topology
            && saved.node == node && saved.boot_id == boot.trim()
            && saved.filesystem == stat.dev() && saved.inode == stat.ino()
            && saved.device == stat.rdev() && saved.owner == stat.uid() && saved.group == stat.gid())
            .ok_or_else(|| invalid(format!("no original ACL baseline for connected Wave:3 node {}; preserve the installation records and review this exact connection before cleanup", node.display())))?;
        result.push((saved, file));
    }
    Ok(result)
}

fn connected_nodes() -> io::Result<Vec<(String, PathBuf)>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(SYS_USB)? {
        let entry = entry?;
        let path = entry.path();
        let vendor = match fs::read_to_string(path.join("idVendor")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if vendor.trim() != "0fd9" || fs::read_to_string(path.join("idProduct"))?.trim() != "0070" {
            continue;
        }
        let bus: u16 =
            fs::read_to_string(path.join("busnum"))?.trim().parse().map_err(io::Error::other)?;
        let number: u16 =
            fs::read_to_string(path.join("devnum"))?.trim().parse().map_err(io::Error::other)?;
        if bus == 0 || bus > 999 || number == 0 || number > 999 {
            return Err(invalid("USB node coordinates are invalid"));
        }
        let topology =
            entry.file_name().into_string().map_err(|_| invalid("USB topology is not UTF-8"))?;
        result.push((topology, PathBuf::from(format!("/dev/bus/usb/{bus:03}/{number:03}"))));
    }
    result.sort();
    Ok(result)
}

fn open_node(path: &Path) -> io::Result<File> {
    // O_PATH obtains metadata authority only. It cannot perform USB I/O.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.file_type().is_char_device() {
        return Err(invalid("USB permission target is not a character device"));
    }
    Ok(file)
}
fn descriptor_path(file: &File) -> PathBuf {
    // Helpers access this process's pinned O_PATH descriptor, never a device
    // pathname that could be replaced after identity validation.
    PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd()))
}
fn read_acl(path: &Path) -> io::Result<String> {
    let output = Command::new("/usr/bin/getfacl")
        .args(["--numeric", "--omit-header", "--no-effective", "--"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("cannot inspect exact USB ACL"));
    }
    canonical_acl(&String::from_utf8(output.stdout).map_err(io::Error::other)?)
}
fn set_acl(path: &Path, acl: &str) -> io::Result<()> {
    let mut child = Command::new("/usr/bin/setfacl")
        .args(["--set-file=-", "--no-mask", "--"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing ACL helper input"))?
        .write_all(acl.as_bytes())?;
    if child.wait()?.success() {
        Ok(())
    } else {
        Err(io::Error::other("exact USB ACL restoration failed"))
    }
}
fn entries(acl: &str) -> io::Result<BTreeMap<(String, String), String>> {
    if acl.len() > 8192 {
        return Err(invalid("ACL is too large"));
    }
    let mut result = BTreeMap::new();
    for line in acl.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() != 3
            || !matches!(fields[0], "user" | "group" | "mask" | "other")
            || (!fields[1].is_empty()
                && (!matches!(fields[0], "user" | "group")
                    || fields[1].parse::<u32>().map_or(true, |id| id.to_string() != fields[1])))
            || fields[2].len() != 3
            || !fields[2]
                .bytes()
                .zip(b"rwx")
                .all(|(actual, allowed)| actual == b'-' || actual == *allowed)
            || result.insert((fields[0].into(), fields[1].into()), fields[2].into()).is_some()
        {
            return Err(invalid("ACL contains an invalid or duplicate field"));
        }
    }
    for kind in ["user", "group", "other"] {
        if !result.contains_key(&(kind.into(), String::new())) {
            return Err(invalid("ACL has no complete permission baseline"));
        }
    }
    if result.keys().any(|(_, id)| !id.is_empty())
        && !result.contains_key(&("mask".into(), String::new()))
    {
        return Err(invalid("extended ACL has no mask"));
    }
    Ok(result)
}
fn render_entries(entries: &BTreeMap<(String, String), String>) -> String {
    let mut result = String::new();
    for kind in ["user", "group", "mask", "other"] {
        for ((tag, id), permissions) in entries.iter().filter(|((tag, _), _)| tag == kind) {
            result.push_str(tag);
            result.push(':');
            result.push_str(id);
            result.push(':');
            result.push_str(permissions);
            result.push('\n');
        }
    }
    result
}
fn canonical_acl(acl: &str) -> io::Result<String> {
    Ok(render_entries(&entries(acl)?))
}
fn granted_acl(original: &str, uid: u32) -> io::Result<String> {
    let mut fields = entries(original)?;
    let prior_mask = fields.get(&("mask".into(), String::new())).cloned();
    if fields.keys().any(|(kind, id)| kind == "user" && !id.is_empty() && id != &uid.to_string()) {
        return Err(invalid(
            "another user's USB ACL is present; the uaccess rule could replace it, so setup is refused",
        ));
    }
    if fields
        .get(&("user".into(), uid.to_string()))
        .is_some_and(|permission| permission.ends_with('x'))
    {
        return Err(invalid("USB user ACL has execute permission; setup is refused"));
    }
    fields.insert(("user".into(), uid.to_string()), "rw-".into());
    let mut mask = *b"---";
    for ((kind, id), value) in &fields {
        if kind == "group" || kind == "user" && !id.is_empty() {
            for (index, permission) in value.bytes().enumerate() {
                if permission != b'-' {
                    mask[index] = permission;
                }
            }
        }
    }
    if let Some(prior_mask) = prior_mask {
        for ((kind, _), permissions) in &fields {
            if kind == "group"
                && permissions
                    .bytes()
                    .zip(prior_mask.bytes())
                    .any(|(permission, allowed)| permission != b'-' && allowed == b'-')
            {
                return Err(invalid(
                    "uaccess would widen another group's effective USB permissions; setup is refused",
                ));
            }
        }
    }
    fields.insert(
        ("mask".into(), String::new()),
        String::from_utf8(mask.to_vec()).map_err(io::Error::other)?,
    );
    Ok(render_entries(&fields))
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests;
