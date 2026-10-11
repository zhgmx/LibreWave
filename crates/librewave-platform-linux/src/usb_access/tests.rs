use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

const BASIC: &str = "user::rw-\ngroup::r--\nother::---\n";

fn snapshot() -> AccessSnapshot {
    AccessSnapshot {
        node: "/dev/bus/usb/001/015".into(),
        topology: "1-8.3".into(),
        boot_id: "00000000-0000-0000-0000-000000000001".into(),
        filesystem: 1,
        inode: 2,
        device: 3,
        owner: 0,
        group: 0,
        grant_uid: 1000,
        original_acl: BASIC.into(),
    }
}

#[test]
fn exact_acl_grant_keeps_base_permissions_and_restores_the_original_mask() {
    assert_eq!(
        granted_acl(BASIC, 1000).expect("grant"),
        "user::rw-\nuser:1000:rw-\ngroup::r--\nmask::rw-\nother::---\n"
    );
    let original = "user::rw-\nuser:1000:r--\ngroup::r--\ngroup:2000:r-x\nmask::r-x\nother::---\n";
    assert_eq!(
        granted_acl(original, 1000).expect("grant with groups"),
        "user::rw-\nuser:1000:rw-\ngroup::r--\ngroup:2000:r-x\nmask::rwx\nother::---\n"
    );
    assert_eq!(canonical_acl(original).expect("original retained"), original);
    assert!(
        granted_acl("user::rw-\nuser:2000:r--\ngroup::r--\nmask::r--\nother::---\n", 1000).is_err()
    );
    assert!(granted_acl("user::rw-\ngroup::rw-\nmask::r--\nother::---\n", 1000).is_err());
    assert!(
        granted_acl("user::rw-\nuser:1000:r-x\ngroup::r--\nmask::r--\nother::---\n", 1000).is_err()
    );
}

#[test]
fn malformed_and_duplicate_permission_records_are_not_defaults() {
    for invalid_acl in [
        "user::rw-\ngroup::r--\n",
        "user::rw-\ngroup::r--\nother::---\nuser::r--\n",
        "user::rw-\ngroup::r--\nother:1000:r--\n",
        "user::rw-\ngroup::r--\nother::rwa\n",
        "user::rw-\nuser:1000:r--\ngroup::r--\nother::---\n",
        "# file: /etc/passwd\nuser::rw-\ngroup::r--\nother::---\n",
    ] {
        assert!(canonical_acl(invalid_acl).is_err(), "{invalid_acl:?}");
    }
    assert!(canonical_acl(&"a".repeat(8193)).is_err());
    let saved = snapshot();
    validate_snapshots(std::slice::from_ref(&saved)).expect("exact snapshot");
    for path in [
        "/etc/passwd",
        "/dev/bus/usb/001/../015",
        "/dev/bus/usb/1/15",
        "/dev/bus/usb/001/015/extra",
    ] {
        let mut forged = saved.clone();
        forged.node = path.into();
        assert!(validate_snapshots(&[forged]).is_err());
    }
    let mut forged = saved.clone();
    forged.topology = "../../etc".into();
    assert!(validate_snapshots(&[forged]).is_err());
    assert!(validate_snapshots(&[saved.clone(), saved]).is_err());
}

#[test]
fn acl_restoration_uses_the_pinned_inode_and_exact_mask() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "librewave-acl-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).expect("create isolated ACL fixture");
    let node = root.join("node");
    fs::write(&node, b"fixture").expect("create fixture");
    fs::set_permissions(&node, fs::Permissions::from_mode(0o640)).expect("baseline mode");
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&node)
        .expect("pin fixture inode without I/O authority");
    let path = descriptor_path(&file);
    let original = read_acl(&path).expect("baseline");
    let grant = granted_acl(&original, 1000).expect("grant model");
    set_acl(&path, &grant).expect("isolated file ACL grant");
    assert_eq!(read_acl(&path).expect("granted ACL"), grant);
    fs::rename(&node, root.join("original-node")).expect("replace pathname");
    fs::write(&node, b"unrelated replacement").expect("replacement file");
    fs::set_permissions(&node, fs::Permissions::from_mode(0o600)).expect("replacement mode");
    set_acl(&path, &original).expect("restore original inode ACL");
    assert_eq!(read_acl(&path).expect("restored ACL"), original);
    assert_eq!(fs::metadata(&node).expect("replacement").mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(root.join("original-node")).expect("original").mode() & 0o777, 0o640);
    set_acl(&path, &original).expect("repeat restoration");
    assert_eq!(read_acl(&path).expect("idempotent restoration"), original);
    drop(file);
    fs::remove_dir_all(root).expect("remove isolated fixture");
}

#[test]
fn privileged_helper_bounds_input_and_requires_invoking_credentials() {
    assert!(restore_access_input(&mut &b"not JSON"[..]).is_err());
    assert!(restore_access_input(&mut &vec![b' '; 65_537][..]).is_err());
    // Ordinary workspace tests have no privilege to restore a real USB ACL.
    if std::env::var_os("PKEXEC_UID").is_none() {
        assert!(restore_access_input(&mut &b"[]"[..]).is_err());
    }
}
