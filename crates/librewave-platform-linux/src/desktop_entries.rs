//! Installed applications from XDG desktop entries.
//!
//! The client lists these so a person can choose an application before it
//! plays. Reading entries changes nothing and uses no audio service.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The most entries one listing returns.
pub const MAX_ENTRIES: usize = 2048;

/// One application that a desktop entry makes visible in menus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledApplication {
    /// The desktop ID, without the `.desktop` suffix.
    pub id: String,
    /// The unlocalized `Name` value.
    pub name: String,
}

/// Lists visible applications from the XDG data directories in precedence
/// order. An earlier directory hides an entry with the same desktop ID.
#[must_use]
pub fn installed_applications() -> Vec<InstalledApplication> {
    let mut seen = HashSet::new();
    let mut applications = Vec::new();
    for directory in application_directories() {
        let mut files = Vec::new();
        collect(&directory, &directory, 0, &mut files);
        files.sort();
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(name) = visible_name(&text) {
                applications.push(InstalledApplication { id, name });
                if applications.len() >= MAX_ENTRIES {
                    break;
                }
            }
        }
    }
    applications.sort_by(|a, b| {
        a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.id.cmp(&b.id))
    });
    applications
}

/// The desktop ID for an entry file, or `None` when it is not a `.desktop`
/// file. Use this for a file that a person chose outside the data directories.
#[must_use]
pub fn desktop_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(".desktop").filter(|id| !id.is_empty()).map(str::to_owned)
}

fn application_directories() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.as_ref().map(|home| home.join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    let mut directories: Vec<PathBuf> = data_home.into_iter().collect();
    directories.extend(data_dirs.split(':').map(PathBuf::from).filter(|path| path.is_absolute()));
    // Flatpak exports, for sessions that did not add them to XDG_DATA_DIRS.
    directories.extend(home.map(|home| home.join(".local/share/flatpak/exports/share")));
    directories.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    let mut unique = HashSet::new();
    directories
        .into_iter()
        .map(|directory| directory.join("applications"))
        .filter(|directory| unique.insert(directory.clone()))
        .collect()
}

/// Collects `(desktop ID, path)` pairs. A subdirectory adds its name and a
/// hyphen to the ID, as the desktop entry specification requires.
fn collect(root: &Path, directory: &Path, depth: usize, files: &mut Vec<(String, PathBuf)>) {
    const MAX_DEPTH: usize = 4;
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                collect(root, &path, depth + 1, files);
            }
        } else if let Some(id) = path
            .strip_prefix(root)
            .ok()
            .and_then(|relative| relative.to_str())
            .and_then(|relative| relative.strip_suffix(".desktop"))
            .filter(|id| !id.is_empty())
        {
            files.push((id.replace('/', "-"), path));
        }
    }
}

/// The `Name` of a visible application entry.
fn visible_name(text: &str) -> Option<String> {
    let mut in_entry = false;
    let (mut name, mut application, mut hidden) = (None, false, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match (key.trim(), value.trim()) {
            ("Name", value) if !value.is_empty() => name = Some(value.to_owned()),
            ("Type", value) => application = value == "Application",
            ("NoDisplay" | "Hidden", "true") => hidden = true,
            _ => {}
        }
    }
    name.filter(|_| application && !hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_visible_application_entries_have_a_name() {
        let entry = "[Desktop Entry]\nType=Application\nName=Spotify\nName[de]=Spotify DE\n\
                     Exec=flatpak run com.spotify.Client\n[Desktop Action New]\nName=New";
        assert_eq!(visible_name(entry).as_deref(), Some("Spotify"));
        assert!(visible_name("[Desktop Entry]\nType=Link\nName=Docs").is_none());
        assert!(
            visible_name("[Desktop Entry]\nType=Application\nName=X\nNoDisplay=true").is_none()
        );
        assert!(visible_name("[Desktop Entry]\nType=Application\nName=X\nHidden=true").is_none());
        assert!(visible_name("[Other]\nType=Application\nName=X").is_none());
    }

    #[test]
    fn desktop_ids_follow_the_specification_and_earlier_directories_win() {
        let root = std::env::temp_dir().join(format!("librewave-entries-{}", std::process::id()));
        let nested = root.join("applications/kde");
        std::fs::create_dir_all(&nested).expect("fixture directory");
        std::fs::write(nested.join("org.test.Player.desktop"), "").expect("fixture entry");
        std::fs::write(root.join("applications/readme.txt"), "").expect("fixture file");
        let mut files = Vec::new();
        collect(&root.join("applications"), &root.join("applications"), 0, &mut files);
        let ids: Vec<_> = files.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["kde-org.test.Player"]);
        std::fs::remove_dir_all(&root).expect("fixture cleanup");
        assert_eq!(
            desktop_id(Path::new("/x/com.spotify.Client.desktop")).as_deref(),
            Some("com.spotify.Client")
        );
        assert!(desktop_id(Path::new("/usr/bin/spotify")).is_none());
        assert!(desktop_id(Path::new("/x/.desktop")).is_none());
    }
}
