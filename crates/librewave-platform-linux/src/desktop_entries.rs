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
    /// The program that `Exec` starts, without its directory. A Flatpak
    /// launcher has none.
    pub executable: Option<String>,
    /// The icon file that the entry names, from the icon theme.
    pub icon: Option<PathBuf>,
}

/// Lists visible applications from the XDG data directories in precedence
/// order. An earlier directory hides an entry with the same desktop ID.
#[must_use]
pub fn installed_applications() -> Vec<InstalledApplication> {
    let mut seen = HashSet::new();
    let mut applications = Vec::new();
    let icons = IconLookup::new();
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
            if let Some(entry) = visible_entry(&text) {
                applications.push(InstalledApplication {
                    id,
                    name: entry.name,
                    executable: entry.exec.as_deref().and_then(executable),
                    icon: entry.icon.as_deref().and_then(|icon| icons.find(icon)),
                });
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

/// The XDG data directories in precedence order, with the Flatpak exports.
fn data_directories() -> Vec<PathBuf> {
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
    directories.retain(|directory| unique.insert(directory.clone()));
    directories
}

fn application_directories() -> Vec<PathBuf> {
    data_directories().into_iter().map(|directory| directory.join("applications")).collect()
}

/// Finds icon files by name in the current theme, then in `hicolor`.
struct IconLookup {
    roots: Vec<PathBuf>,
    pixmaps: Vec<PathBuf>,
}

impl IconLookup {
    // Sizes near the 20 to 40 px the client draws come first.
    const SIZES: [&str; 8] = ["48", "64", "32", "128", "96", "256", "24", "512"];

    fn new() -> Self {
        let data = data_directories();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut bases: Vec<PathBuf> = home.iter().map(|home| home.join(".icons")).collect();
        bases.extend(data.iter().map(|directory| directory.join("icons")));
        let mut themes = Vec::new();
        if let Some(home) = &home {
            themes.extend(setting(&home.join(".config/kdeglobals"), "Icons", "Theme"));
            themes.extend(setting(
                &home.join(".config/gtk-3.0/settings.ini"),
                "Settings",
                "gtk-icon-theme-name",
            ));
        }
        themes.extend(["breeze", "Adwaita", "hicolor"].map(str::to_owned));
        let mut unique = HashSet::new();
        themes.retain(|theme| unique.insert(theme.clone()));
        let roots = themes
            .iter()
            .flat_map(|theme| bases.iter().map(move |base| base.join(theme)))
            .filter(|root| root.is_dir())
            .collect();
        let pixmaps = data.iter().map(|directory| directory.join("pixmaps")).collect();
        Self { roots, pixmaps }
    }

    fn find(&self, icon: &str) -> Option<PathBuf> {
        let path = Path::new(icon);
        if path.is_absolute() {
            return path.is_file().then(|| path.to_path_buf());
        }
        if icon.contains('/') || icon.is_empty() {
            return None;
        }
        // An entry may name the file with its extension.
        let name = icon.strip_suffix(".png").or_else(|| icon.strip_suffix(".svg")).unwrap_or(icon);
        for root in &self.roots {
            for size in Self::SIZES {
                for directory in [format!("{size}x{size}/apps"), format!("apps/{size}")] {
                    for extension in ["png", "svg"] {
                        let file = root.join(&directory).join(format!("{name}.{extension}"));
                        if file.is_file() {
                            return Some(file);
                        }
                    }
                }
            }
            for directory in ["scalable/apps", "apps/scalable"] {
                let file = root.join(directory).join(format!("{name}.svg"));
                if file.is_file() {
                    return Some(file);
                }
            }
        }
        self.pixmaps
            .iter()
            .flat_map(|directory| {
                ["png", "svg"].map(|extension| directory.join(format!("{name}.{extension}")))
            })
            .find(|file| file.is_file())
    }
}

/// One `key=value` from a section of an INI-style settings file.
fn setting(path: &Path, section: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == format!("[{section}]");
        } else if inside
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// The program name that an `Exec` line starts.
fn executable(exec: &str) -> Option<String> {
    let program = exec.split_whitespace().map(|word| word.trim_matches('"')).find(|word| {
        let assignment = word.contains('=') && !word.starts_with('/');
        *word != "env" && !assignment
    })?;
    let name = Path::new(program).file_name()?.to_str()?;
    (name != "flatpak" && !name.is_empty() && !name.starts_with('%')).then(|| name.to_owned())
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

struct Entry {
    name: String,
    exec: Option<String>,
    icon: Option<String>,
}

/// The fields of a visible application entry.
fn visible_entry(text: &str) -> Option<Entry> {
    let mut in_entry = false;
    let (mut name, mut application, mut hidden) = (None, false, false);
    let (mut exec, mut icon) = (None, None);
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
            ("Exec", value) if !value.is_empty() => exec = Some(value.to_owned()),
            ("Icon", value) if !value.is_empty() => icon = Some(value.to_owned()),
            ("Type", value) => application = value == "Application",
            ("NoDisplay" | "Hidden", "true") => hidden = true,
            _ => {}
        }
    }
    name.filter(|_| application && !hidden).map(|name| Entry { name, exec, icon })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_visible_application_entries_have_a_name() {
        let entry = "[Desktop Entry]\nType=Application\nName=Spotify\nName[de]=Spotify DE\n\
                     Exec=flatpak run com.spotify.Client\n[Desktop Action New]\nName=New";
        let name = |text| visible_entry(text).map(|entry| entry.name);
        assert_eq!(name(entry).as_deref(), Some("Spotify"));
        assert!(name("[Desktop Entry]\nType=Link\nName=Docs").is_none());
        assert!(name("[Desktop Entry]\nType=Application\nName=X\nNoDisplay=true").is_none());
        assert!(name("[Desktop Entry]\nType=Application\nName=X\nHidden=true").is_none());
        assert!(name("[Other]\nType=Application\nName=X").is_none());
        let fields = visible_entry(entry).expect("entry");
        assert_eq!(fields.icon, None);
        assert_eq!(fields.exec.as_deref(), Some("flatpak run com.spotify.Client"));
    }

    #[test]
    fn exec_lines_give_the_program_name_except_for_flatpak_launchers() {
        assert_eq!(executable("/usr/bin/firefox %u").as_deref(), Some("firefox"));
        assert_eq!(executable("env GDK_BACKEND=x11 \"spotify\" %U").as_deref(), Some("spotify"));
        assert!(executable("/usr/bin/flatpak run --branch=stable com.spotify.Client").is_none());
        assert!(executable("").is_none());
    }

    #[test]
    fn icons_resolve_from_themes_absolute_paths_and_pixmaps() {
        let root = std::env::temp_dir().join(format!("librewave-icons-{}", std::process::id()));
        let theme = root.join("hicolor");
        std::fs::create_dir_all(theme.join("48x48/apps")).expect("fixture theme");
        std::fs::create_dir_all(root.join("pixmaps")).expect("fixture pixmaps");
        std::fs::write(theme.join("48x48/apps/org.test.Player.png"), "").expect("theme icon");
        std::fs::write(root.join("pixmaps/legacy.svg"), "").expect("pixmap");
        let lookup = IconLookup { roots: vec![theme.clone()], pixmaps: vec![root.join("pixmaps")] };
        assert_eq!(
            lookup.find("org.test.Player"),
            Some(theme.join("48x48/apps/org.test.Player.png"))
        );
        assert_eq!(lookup.find("org.test.Player.png"), lookup.find("org.test.Player"));
        assert_eq!(lookup.find("legacy"), Some(root.join("pixmaps/legacy.svg")));
        let absolute = root.join("pixmaps/legacy.svg");
        assert_eq!(lookup.find(absolute.to_str().expect("path")), Some(absolute.clone()));
        assert!(lookup.find("missing").is_none());
        assert!(lookup.find("../escape").is_none());
        std::fs::remove_dir_all(&root).expect("fixture cleanup");
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
