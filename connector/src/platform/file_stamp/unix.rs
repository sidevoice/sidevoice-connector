use std::os::unix::fs::MetadataExt;

pub struct Os;

impl super::FileIdentities for Os {
    fn change(_: &std::path::Path, metadata: &std::fs::Metadata) -> std::io::Result<[u64; 4]> {
        Ok([
            metadata.dev(),
            metadata.ino(),
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
        ])
    }
}
