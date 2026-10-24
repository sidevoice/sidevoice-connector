use std::os::unix::fs::MetadataExt;

pub struct Os;

impl super::FileIdentities for Os {
    fn change(metadata: &std::fs::Metadata) -> [u64; 4] {
        [
            metadata.dev(),
            metadata.ino(),
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
        ]
    }
}
