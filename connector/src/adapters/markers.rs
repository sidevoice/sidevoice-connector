//! Passive install markers. This module only inspects filesystem metadata; it never starts an app or command.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Root {
    Absolute,
    Home,
    LocalAppData,
    ProgramFiles,
}

#[derive(Clone, Copy, Debug)]
pub struct Marker {
    pub os: &'static str,
    pub kind: &'static str,
    pub root: Root,
    pub path: &'static str,
}

pub struct Roots {
    pub home: PathBuf,
    pub local_app_data: Option<PathBuf>,
    pub program_files: Option<PathBuf>,
}

impl Roots {
    pub fn process(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            program_files: std::env::var_os("ProgramFiles").map(PathBuf::from),
        }
    }

    fn resolve(&self, marker: &Marker) -> Option<PathBuf> {
        match marker.root {
            Root::Absolute => Some(PathBuf::from(marker.path)),
            Root::Home => Some(self.home.join(marker.path)),
            Root::LocalAppData => self.local_app_data.as_ref().map(|root| root.join(marker.path)),
            Root::ProgramFiles => self.program_files.as_ref().map(|root| root.join(marker.path)),
        }
    }
}

/// Return only matched kinds. Paths remain local and never enter an agent response or log.
pub fn matching_kinds(markers: &[Marker], os: &str, roots: &Roots) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    for marker in markers.iter().filter(|marker| marker.os == os) {
        if roots
            .resolve(marker)
            .is_some_and(|path| fs::symlink_metadata(path).is_ok())
            && !kinds.contains(&marker.kind)
        {
            kinds.push(marker.kind);
        }
    }
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn path_markers_check_metadata_without_starting_the_target() {
        let root = std::env::temp_dir().join(format!("sidevoice-marker-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        fs::create_dir_all(home.join("Applications")).unwrap();
        let marker_file = home.join("Applications/Cursor.app");
        let launched = root.join("launched");
        fs::write(&marker_file, format!("#!/bin/sh\ntouch '{}'\n", launched.display())).unwrap();
        fs::set_permissions(&marker_file, fs::Permissions::from_mode(0o700)).unwrap();
        let markers = [Marker {
            os: "macos",
            kind: "app_bundle",
            root: Root::Home,
            path: "Applications/Cursor.app",
        }];
        let roots = Roots::process(&home);
        assert_eq!(matching_kinds(&markers, "macos", &roots), vec!["app_bundle"]);
        assert!(!launched.exists(), "path discovery launched the app");
        assert!(matching_kinds(&markers, "linux", &roots).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_roots_are_injected_and_only_matching_kinds_leave_detection() {
        let root = std::env::temp_dir().join(format!("sidevoice-marker-{}", uuid::Uuid::new_v4()));
        let local = root.join("LocalAppData");
        let program = root.join("ProgramFiles");
        fs::create_dir_all(local.join("Programs/cursor")).unwrap();
        fs::create_dir_all(&program).unwrap();
        let markers = [
            Marker { os: "windows", kind: "install_dir", root: Root::LocalAppData, path: "Programs/cursor" },
            Marker { os: "windows", kind: "install_dir", root: Root::ProgramFiles, path: "cursor" },
        ];
        let roots = Roots { home: root.clone(), local_app_data: Some(local), program_files: Some(program) };
        assert_eq!(matching_kinds(&markers, "windows", &roots), vec!["install_dir"]);
        fs::remove_dir_all(root).unwrap();
    }
}
