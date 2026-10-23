//! Windows: a copy of this test binary with the part appended after its image, then its length and a mark. The
//! loader maps the image and ignores what follows it; the stand-in reads its own end back.

use crate::platform::fs::{Files, TestFiles};
use std::io::{Read, Seek, SeekFrom, Write};

/// What ends a stand-in: after the part and its length (8 bytes, little-endian).
const MARK: &[u8; 8] = b"SVSTAND1";
/// The longest part read back.
const LONGEST: u64 = 1 << 20;

/// This OS's stand-ins.
pub struct Os;

impl super::StandIns for Os {
    fn make(program: &std::path::Path, part: &str) {
        std::fs::copy(std::env::current_exe().unwrap(), program).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(program)
            .unwrap();
        file.write_all(part.as_bytes()).unwrap();
        file.write_all(&(part.len() as u64).to_le_bytes()).unwrap();
        file.write_all(MARK).unwrap();
        file.sync_all().unwrap();
        drop(file);
        Files::shut_others_out(program);
    }

    fn part() -> Option<String> {
        let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
        let mut tail = [0u8; 16];
        file.seek(SeekFrom::End(-16)).ok()?;
        file.read_exact(&mut tail).ok()?;
        if &tail[8..] != MARK {
            return None;
        }
        let length = u64::from_le_bytes(tail[..8].try_into().ok()?);
        if length > LONGEST {
            return None;
        }
        file.seek(SeekFrom::End(-16 - length as i64)).ok()?;
        let mut part = vec![0; length as usize];
        file.read_exact(&mut part).ok()?;
        String::from_utf8(part).ok()
    }
}
