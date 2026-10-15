//! Deletion is confined to the two paths the installer itself defines below HOME.

use crate::service::layout::Layout;
use std::fs;
use std::path::{Component, Path, PathBuf};

fn clean_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

pub(super) fn validate_scope(layout: &Layout) -> Result<(), &'static str> {
    let home = &layout.home;
    if !clean_absolute(home)
        || home == Path::new("/")
        || fs::canonicalize(home).ok().as_deref() != Some(home)
    {
        return Err("unsafe home for uninstall");
    }
    let data = home.join(".sidevoice");
    let data_home = home.join(".local/share");
    let releases = data_home.join("sidevoice");
    if !clean_absolute(&layout.data)
        || !clean_absolute(&layout.data_home)
        || !clean_absolute(&layout.releases)
        || layout.data != data
        || layout.data_home != data_home
        || layout.releases != releases
    {
        return Err("uninstall path is outside known Sidevoice roots");
    }
    for path in [&layout.data, &layout.releases] {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err("uninstall root is not a real directory")
            }
            Ok(_) if fs::canonicalize(path).ok().as_deref() != Some(path) => {
                return Err("uninstall root is not canonical")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot inspect uninstall root"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_owned_roots_can_be_uninstalled() {
        let scratch = crate::secure_fs::tests::Scratch::new("uninstall-scope");
        let home = scratch.0.join("home");
        fs::create_dir(&home).unwrap();
        let mut layout = Layout::from_vars(
            |name| (name == "HOME").then(|| home.as_os_str().to_owned()),
            std::iter::empty(),
            &[],
        )
        .unwrap();
        assert!(validate_scope(&layout).is_ok());
        for unsafe_path in [
            PathBuf::new(),
            PathBuf::from("relative"),
            PathBuf::from("/"),
            home.clone(),
            home.join(".sidevoice/.."),
            home.join("elsewhere"),
        ] {
            layout.data = unsafe_path;
            assert!(validate_scope(&layout).is_err());
        }
    }
}
