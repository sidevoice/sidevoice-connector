//! Remove only legacy voice-room copies carrying Sidevoice's installation marker.
use crate::{agents::message, proof::Profile};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::os::unix::fs::MetadataExt;
use std::{
    fs,
    path::{Path, PathBuf},
};
const MARKER: &str = "sidevoice: installed copy";

fn target_state(target: &Path) -> Result<&'static str> {
    let metadata = match fs::symlink_metadata(target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("absent"),
        Err(error) => return Err(error.into()),
        Ok(metadata) => metadata,
    };
    // A marker in the destination of a symlink never makes the link an installed copy.
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Ok("foreign");
    }
    let manifest = target.join("SKILL.md");
    let metadata = match fs::symlink_metadata(&manifest) {
        Ok(value) => value,
        Err(_) => return Ok("foreign"),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > 1024 * 1024
    {
        return Ok("foreign");
    }
    Ok(
        if fs::read_to_string(manifest).is_ok_and(|text| text.contains(MARKER)) {
            "installed"
        } else {
            "foreign"
        },
    )
}

pub fn run_cli(profile: &Profile, argv: &[String]) -> Result<Value> {
    let Some(action) = argv
        .first()
        .map(String::as_str)
        .filter(|s| matches!(*s, "status" | "remove"))
    else {
        bail!("{}", message("skill.usage", &json!({})));
    };
    let mut directory = profile.claude.join("skills");
    let mut args = argv[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => {
                let Some(value) = args.next() else {
                    bail!("{}", message("skill.usage", &json!({})));
                };
                let path = PathBuf::from(value);
                directory = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()?.join(path)
                };
            }
            "--json" => {}
            _ => bail!("{}", message("skill.usage", &json!({}))),
        }
    }
    let target = directory.join("voice-room");
    let state = target_state(&target)?;
    if action == "status" {
        return Ok(json!({"state":state,"target":target}));
    }
    if state == "foreign" {
        bail!("{}", message("skill.foreign", &json!({"target":target})));
    }
    if state == "installed" {
        // Recheck after canonicalizing the parent, so an exchanged parent cannot redirect removal.
        let parent = directory.canonicalize()?;
        let selected = parent.join("voice-room");
        if target_state(&selected)? != "installed" {
            bail!("{}", message("skill.foreign", &json!({"target":target})));
        }
        fs::remove_dir_all(selected)?;
    }
    Ok(
        json!({"state":"absent","target":target,"action":if state == "installed" {"removed"} else {"nothing to remove"}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn foreign_and_symlinked_skill_are_not_owned() {
        let root = std::env::temp_dir().join(format!("sidevoice-skill-{}", uuid::Uuid::new_v4()));
        let target = root.join("voice-room");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "personal skill").unwrap();
        assert_eq!(target_state(&target).unwrap(), "foreign");
        fs::write(target.join("SKILL.md"), MARKER).unwrap();
        assert_eq!(target_state(&target).unwrap(), "installed");
        symlink(&target, root.join("linked")).unwrap();
        assert_eq!(target_state(&root.join("linked")).unwrap(), "foreign");
        fs::remove_dir_all(root).unwrap();
    }
}

pub fn cli_text(result: &Value) -> String {
    message(
        "skill.result",
        &json!({"state":result.get("action").unwrap_or(&result["state"]),"target":result["target"]}),
    )
}
