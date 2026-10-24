use std::os::windows::fs::MetadataExt;

pub struct Os;

impl super::FileIdentities for Os {
    fn change(metadata: &std::fs::Metadata) -> [u64; 4] {
        // Available from std metadata without opening another handle. std's file_index is unstable.
        [metadata.creation_time(), 0, 0, 0]
    }
}
