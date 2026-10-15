//! Desktop app registration, queried through each OS rather than guessed install directories.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
#[allow(dead_code)] // Fields for the other operating systems are exercised by fixture tests.
pub struct Identity {
    pub mac_bundle: &'static str,
    pub windows_name: &'static str,
    pub windows_publisher: &'static str,
    pub linux_desktop: &'static str,
}

/// The exact LaunchServices query arguments (AppleScript's `path to application id`).
#[cfg(any(target_os = "macos", test))]
pub fn mac_query(identity: Identity) -> Vec<String> {
    vec![
        "-e".into(),
        format!(
            "POSIX path of (path to application id \"{}\")",
            identity.mac_bundle
        ),
    ]
}

#[cfg(any(target_os = "macos", test))]
pub fn mac_result(output: &str) -> Option<PathBuf> {
    let path = PathBuf::from(output.trim());
    (path.is_absolute()
        && path.extension().is_some_and(|extension| extension == "app")
        && path.is_dir())
    .then_some(path)
}

/// Parse the values of one `reg.exe query /s` key; a matching name alone is insufficient.
#[cfg(any(target_os = "windows", test))]
pub fn windows_results(output: &str, identity: Identity) -> Vec<PathBuf> {
    let mut matches = Vec::new();
    let mut name = String::new();
    let mut publisher = String::new();
    let mut location = String::new();
    let mut finish = |name: &str, publisher: &str, location: &str| {
        if name.eq_ignore_ascii_case(identity.windows_name)
            && publisher.eq_ignore_ascii_case(identity.windows_publisher)
            && !location.is_empty()
        {
            matches.push(PathBuf::from(location));
        }
    };
    for line in output.lines().chain(std::iter::once("HKEY_END")) {
        let trimmed = line.trim();
        if trimmed.starts_with("HKEY_") || trimmed.starts_with("HKEY_END") {
            finish(&name, &publisher, &location);
            name.clear();
            publisher.clear();
            location.clear();
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let Some(kind) = parts.next() else { continue };
        if kind != "REG_SZ" && kind != "REG_EXPAND_SZ" {
            continue;
        }
        let value = parts.collect::<Vec<_>>().join(" ");
        match key {
            "DisplayName" => name = value,
            "Publisher" => publisher = value,
            "InstallLocation" => location = value,
            _ => {}
        }
    }
    matches
}

/// XDG's first desktop file of this ID wins, including a `Hidden=true` tombstone.
pub fn linux_entry(identity: Identity, roots: &[PathBuf], path: Option<&str>) -> Option<PathBuf> {
    for root in roots {
        let file = root.join("applications").join(identity.linux_desktop);
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        if desktop_valid(&contents, path) {
            return Some(file);
        }
        return None;
    }
    None
}

fn desktop_valid(contents: &str, path: Option<&str>) -> bool {
    let mut in_entry = false;
    let mut kind = None;
    let mut hidden = false;
    let mut exec = None;
    let mut dbus = false;
    let mut try_exec = None;
    for line in contents.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "Type" => kind = Some(value),
                "Hidden" => hidden = value == "true",
                "Exec" => exec = Some(value),
                "DBusActivatable" => dbus = value == "true",
                "TryExec" => try_exec = Some(value),
                _ => {}
            }
        }
    }
    kind == Some("Application")
        && !hidden
        && (exec.is_some_and(|value| !value.trim().is_empty()) || dbus)
        && try_exec.is_none_or(|value| {
            if Path::new(value).is_absolute() {
                crate::agents::executable(value)
            } else {
                crate::agents::find_in_path(&[value], path).is_some()
            }
        })
}

pub fn linux_roots(user_data: &Path) -> Vec<PathBuf> {
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    std::iter::once(user_data.to_path_buf())
        .chain(
            system
                .split(':')
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    const CURSOR: Identity = Identity {
        mac_bundle: "com.todesktop.230313mzl4w4u92",
        windows_name: "Cursor",
        windows_publisher: "Anysphere",
        linux_desktop: "cursor.desktop",
    };

    #[test]
    fn mac_query_uses_bundle_id_and_refuses_unresolved_or_missing_paths() {
        assert_eq!(
            mac_query(CURSOR),
            [
                "-e",
                "POSIX path of (path to application id \"com.todesktop.230313mzl4w4u92\")"
            ]
        );
        assert_eq!(mac_result("/Applications/Cursor.app"), None);
        assert_eq!(mac_result("not found"), None);
    }

    #[test]
    fn windows_registry_requires_exact_name_publisher_and_location() {
        let output = "HKEY_CURRENT_USER\\Software\\Uninstall\\wrong\n    DisplayName    REG_SZ    Cursor\n    Publisher    REG_SZ    Other\n    InstallLocation    REG_SZ    C:\\Wrong\nHKEY_CURRENT_USER\\Software\\Uninstall\\right\n    DisplayName    REG_SZ    Cursor\n    Publisher    REG_SZ    Anysphere\n    InstallLocation    REG_SZ    C:\\Cursor\n";
        assert_eq!(
            windows_results(output, CURSOR),
            [PathBuf::from("C:\\Cursor")]
        );
        assert!(windows_results("ERROR: Access is denied", CURSOR).is_empty());
    }

    #[test]
    fn linux_desktop_entry_uses_xdg_priority_and_rejects_stale_or_hidden() {
        let root = std::env::temp_dir().join(format!("sidevoice-app-{}", Uuid::new_v4()));
        let user = root.join("user");
        let system = root.join("system");
        std::fs::create_dir_all(user.join("applications")).unwrap();
        std::fs::create_dir_all(system.join("applications")).unwrap();
        let file = system.join("applications/cursor.desktop");
        std::fs::write(
            &file,
            "[Desktop Entry]\nType=Application\nName=Cursor\nExec=cursor %F\n",
        )
        .unwrap();
        assert_eq!(
            linux_entry(CURSOR, &[user.clone(), system.clone()], None),
            Some(file)
        );
        let user_file = user.join("applications/cursor.desktop");
        std::fs::write(
            &user_file,
            "[Desktop Entry]\nType=Application\nHidden=true\nExec=cursor\n",
        )
        .unwrap();
        assert_eq!(
            linux_entry(CURSOR, &[user.clone(), system.clone()], None),
            None
        );
        std::fs::write(
            &user_file,
            "[Desktop Entry]\nType=Application\nExec=cursor\nTryExec=/missing/cursor\n",
        )
        .unwrap();
        assert_eq!(linux_entry(CURSOR, &[user, system], None), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
