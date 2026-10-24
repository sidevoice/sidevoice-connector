use std::fs::{Metadata, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

pub struct Os;

impl super::FileIdentities for Os {
    fn change(path: &Path, metadata: &Metadata) -> io::Result<[u64; 4]> {
        // Windows can preserve creation time when a path is replaced (file-system tunneling).
        // Read the file index too, using the same maintained wrapper as platform/uninstall.
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)?;
        let info = winapi_util::file::information(&file)?;
        Ok([
            metadata.creation_time(),
            info.volume_serial_number(),
            info.file_index(),
            0,
        ])
    }
}
