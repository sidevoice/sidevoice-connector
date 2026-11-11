//! Windows: one `RunOnce` entry per folder under the user's own `HKCU`, which Windows runs and removes at the next
//! sign-in. No elevation is needed, as `MoveFileEx(MOVEFILE_DELAY_UNTIL_REBOOT)` would need.

use std::io;
use std::path::PathBuf;
use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_ONCE: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";

/// This OS's deferred deletion.
pub struct Os;

impl super::Deferred for Os {
    fn at_next_sign_in(folders: &[PathBuf]) -> io::Result<()> {
        // Every command is checked before the first entry is written: all or nothing for an unsafe path.
        let entries = folders
            .iter()
            .map(|folder| {
                let command = super::removal_command(folder).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} cannot be deleted later safely", folder.display()),
                    )
                })?;
                // Named by the folder (a claimed release has a unique name), so asking twice for one folder is one
                // entry, not two.
                let name = folder
                    .file_name()
                    .map(|name| format!("SidevoiceRemove-{}", name.to_string_lossy()))
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                Ok((name, command))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let (key, _) =
            RegKey::predef(HKEY_CURRENT_USER).create_subkey_with_flags(RUN_ONCE, KEY_SET_VALUE)?;
        for (name, command) in entries {
            key.set_value(name, &command)?;
        }
        Ok(())
    }
}
