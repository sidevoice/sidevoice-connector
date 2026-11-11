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
        let commands = folders
            .iter()
            .map(|folder| {
                super::removal_command(folder).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} cannot be deleted later safely", folder.display()),
                    )
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let (key, _) =
            RegKey::predef(HKEY_CURRENT_USER).create_subkey_with_flags(RUN_ONCE, KEY_SET_VALUE)?;
        for command in commands {
            key.set_value(
                format!("SidevoiceRemove{}", uuid::Uuid::new_v4().simple()),
                &command,
            )?;
        }
        Ok(())
    }
}
