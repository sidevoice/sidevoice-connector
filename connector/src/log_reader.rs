//! The connector's own log read in pages, for the core's `logs.read` request: whole lines only, and a cursor that
//! survives rotation.
//!
//! This is the core's reader (`rust/server/logs/reader.rs` in sidevoice-core) with one change: the page's soft size is
//! the caller's, because a page travels as one frame of the core link (1 MiB) and not as a response body. Both are held
//! to the same list of behaviours (`tests`), so they change together.
//!
//! A cursor is a generation (the hash of its first line, so a rename or the copy-and-truncate of [`crate::logfile`]
//! does not move it and no path, inode or pid is ever shown) and an offset in it, always just after a newline. A
//! line's id is the cursor just before it. A generation is a file of the log; they are given oldest first, and a file
//! that is not there is skipped.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::platform::fs::{Files, Fs};
use serde_json::{json, Value};

pub const DEFAULT_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 1000;
/// A page's soft size: lines are added while they fit, the first one whatever its size. Half the core's reader's:
/// a page travels as one frame of the core link (1 MiB at most) and JSON can write a byte as six (`\u0001`).
const PAGE_BYTES: usize = 128 * 1024;
const MAX_CURSOR: usize = 128;
/// The cursor of the beginning of the log: the answer for a log with no line yet.
const START: &str = "start";

#[derive(Serialize)]
pub struct Line {
    pub id: String,
    pub text: String,
}

#[derive(Serialize)]
pub struct Page {
    pub lines: Vec<Line>,
    pub cursor: String,
    pub more: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The cursor is not one this reader gives.
    Invalid,
    /// The cursor's generation is gone, or the file is shorter than the cursor says.
    Expired,
    Unreadable,
}

impl From<io::Error> for Refused {
    fn from(_: io::Error) -> Self {
        Self::Unreadable
    }
}

/// The latest `limit` lines when `after` is `None`, else the lines written after the cursor `after`, oldest first.
pub fn read(paths: &[PathBuf], after: Option<&str>, limit: usize) -> Result<Page, Refused> {
    pages(open(paths)?, after, limit)
}

/// The files that exist, newest first: a rotation between two opens then shows a file twice (told apart by its first
/// line) and hides none. A link, or a file that is not private, is refused.
fn open(paths: &[PathBuf]) -> io::Result<Vec<File>> {
    let mut files = Vec::new();
    for path in paths.iter().rev() {
        let file = match Files::open_read(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        Files::check_private(&file)?;
        files.push(file);
    }
    Ok(files)
}

// From here on the reader is the one the desktop app has for its own log (desktop/core/src/app_log.rs): both are held
// to the same list of behaviours, so they change together.

/// [`read`] over `files`, newest first.
fn pages(files: Vec<File>, after: Option<&str>, limit: usize) -> Result<Page, Refused> {
    let cursor = after.map(Cursor::parse).transpose()?;
    let generations = generations(files)?;
    let (lines, last, more) = match cursor {
        None => {
            let (lines, last) = tail(&generations, limit)?;
            (lines, last, false)
        }
        Some(Cursor::Start) => forward(&generations, 0, 0, limit)?,
        Some(Cursor::At(id, offset)) => {
            let index = generations
                .iter()
                .position(|generation| generation.id == id)
                .ok_or(Refused::Expired)?;
            let generation = &generations[index];
            if offset > generation.end {
                return Err(Refused::Expired);
            }
            if offset > 0 && !generation.after_newline(offset)? {
                return Err(Refused::Invalid);
            }
            forward(&generations, index, offset, limit)?
        }
    };
    Ok(Page {
        lines,
        // A page with no line leaves the cursor where it was.
        cursor: last.unwrap_or_else(|| after.unwrap_or(START).to_owned()),
        more,
    })
}

enum Cursor {
    Start,
    At(u64, u64),
}

impl Cursor {
    fn parse(text: &str) -> Result<Self, Refused> {
        if text == START {
            return Ok(Self::Start);
        }
        let digits = |part: &str, hex: bool| {
            !part.is_empty()
                && part.bytes().all(|byte| {
                    if hex {
                        byte.is_ascii_hexdigit()
                    } else {
                        byte.is_ascii_digit()
                    }
                })
        };
        match text.split_once('.') {
            Some((id, offset))
                if text.len() <= MAX_CURSOR
                    && id.len() == 16
                    && digits(id, true)
                    && digits(offset, false) =>
            {
                let id = u64::from_str_radix(id, 16).map_err(|_| Refused::Invalid)?;
                let offset = offset.parse().map_err(|_| Refused::Invalid)?;
                Ok(Self::At(id, offset))
            }
            _ => Err(Refused::Invalid),
        }
    }
}

fn at(id: u64, offset: u64) -> String {
    format!("{id:016x}.{offset}")
}

/// A generation as it was when opened: `end` is just after its last complete line.
struct Generation {
    file: File,
    id: u64,
    end: u64,
}

/// A line found, with the offset it ends at and its size in the file.
struct Found {
    line: Line,
    end: u64,
    size: usize,
}

impl Generation {
    /// `None` while it has no complete line: it has no identity yet.
    fn of(file: File) -> io::Result<Option<Self>> {
        let end = complete_end(&file)?;
        if end == 0 {
            return Ok(None);
        }
        let mut first = Vec::new();
        (&file).seek(SeekFrom::Start(0))?;
        BufReader::new((&file).take(end)).read_until(b'\n', &mut first)?;
        first.pop();
        let digest = Sha256::digest(&first);
        let id = u64::from_be_bytes(digest[..8].try_into().expect("eight bytes"));
        Ok(Some(Self { file, id, end }))
    }

    fn after_newline(&self, offset: u64) -> io::Result<bool> {
        let mut byte = [0u8];
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset - 1))?;
        file.read_exact(&mut byte)?;
        Ok(byte[0] == b'\n')
    }

    /// The first line start at or after `offset`, or the end.
    fn line_start_from(&self, offset: u64) -> io::Result<u64> {
        if offset == 0 {
            return Ok(0);
        }
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut reader = BufReader::new(file.take(self.end.saturating_sub(offset - 1)));
        let mut skipped = Vec::new();
        Ok(offset - 1 + reader.read_until(b'\n', &mut skipped)? as u64)
    }

    /// The complete lines from `from` (a line start): at most `max_lines` and `max_bytes`, the first line however big
    /// when `first_free`.
    fn lines_from(
        &self,
        from: u64,
        max_lines: usize,
        max_bytes: usize,
        first_free: bool,
    ) -> io::Result<Vec<Found>> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(from))?;
        let mut reader = BufReader::new(file.take(self.end.saturating_sub(from)));
        let (mut lines, mut offset, mut bytes, mut raw) = (Vec::new(), from, 0usize, Vec::new());
        while lines.len() < max_lines {
            raw.clear();
            if reader.read_until(b'\n', &mut raw)? == 0
                || (bytes + raw.len() > max_bytes && !(first_free && lines.is_empty()))
            {
                break;
            }
            let text = raw.strip_suffix(b"\n").unwrap_or(&raw);
            let text = text.strip_suffix(b"\r").unwrap_or(text);
            lines.push(Found {
                line: Line {
                    id: at(self.id, offset),
                    text: String::from_utf8_lossy(text).into_owned(),
                },
                end: offset + raw.len() as u64,
                size: raw.len(),
            });
            offset += raw.len() as u64;
            bytes += raw.len();
        }
        Ok(lines)
    }
}

/// The offset just after the last newline of the file: a last line still being written is not part of it yet.
fn complete_end(mut file: &File) -> io::Result<u64> {
    let mut end = file.metadata()?.len();
    let mut chunk = [0u8; 4096];
    while end > 0 {
        let start = end.saturating_sub(chunk.len() as u64);
        let part = &mut chunk[..(end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(part)?;
        if let Some(newline) = part.iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + newline as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// The generations that have a line, oldest first, of `files` newest first.
fn generations(files: Vec<File>) -> io::Result<Vec<Generation>> {
    let mut found = Vec::new();
    for file in files {
        found.extend(Generation::of(file)?);
    }
    found.reverse();
    found.dedup_by(|later, kept| {
        let same = later.id == kept.id;
        if same {
            kept.end = kept.end.max(later.end);
        }
        same
    });
    Ok(found)
}

/// The lines after `offset` in generation `first` and then in each newer one: up to `limit` lines and a page's size.
/// With the cursor after the last of them, and whether more is already there.
fn forward(
    generations: &[Generation],
    first: usize,
    offset: u64,
    limit: usize,
) -> io::Result<(Vec<Line>, Option<String>, bool)> {
    let (mut lines, mut cursor, mut bytes) = (Vec::new(), None, 0);
    for (index, generation) in generations.iter().enumerate().skip(first) {
        let from = if index == first { offset } else { 0 };
        let read = generation.lines_from(
            from,
            limit - lines.len(),
            PAGE_BYTES.saturating_sub(bytes),
            lines.is_empty(),
        )?;
        let reached = read.last().map_or(from, |last| last.end);
        if let Some(last) = read.last() {
            cursor = Some(at(generation.id, last.end));
        }
        bytes += read.iter().map(|line| line.size).sum::<usize>();
        lines.extend(read.into_iter().map(|read| read.line));
        if lines.len() == limit || reached < generation.end {
            let more = reached < generation.end || index + 1 < generations.len();
            return Ok((lines, cursor, more));
        }
    }
    Ok((lines, cursor, false))
}

/// The latest lines, at most `limit` and a page's size, oldest first, with the cursor after the last.
fn tail(generations: &[Generation], limit: usize) -> io::Result<(Vec<Line>, Option<String>)> {
    let (mut newest_first, mut cursor, mut bytes) = (Vec::new(), None, 0);
    'older: for generation in generations.iter().rev() {
        if newest_first.len() == limit {
            break;
        }
        // A window of the end of the generation; wider only when it holds no whole line.
        let mut window = PAGE_BYTES.saturating_sub(bytes).max(1) as u64;
        let (from, read) = loop {
            let from = generation.line_start_from(generation.end.saturating_sub(window))?;
            let read = generation.lines_from(from, usize::MAX, usize::MAX, true)?;
            if from == 0 || !read.is_empty() {
                break (from, read);
            }
            window *= 2;
        };
        for line in read.into_iter().rev() {
            if newest_first.len() == limit
                || (!newest_first.is_empty() && bytes + line.size > PAGE_BYTES)
            {
                break 'older;
            }
            if newest_first.is_empty() {
                cursor = Some(at(generation.id, line.end));
            }
            bytes += line.size;
            newest_first.push(line.line);
        }
        // What is older than the window is out of the page's size.
        if from > 0 {
            break;
        }
    }
    newest_first.reverse();
    Ok((newest_first, cursor))
}

/// The most a page may weigh as the core link's frame (1 MiB at most, with the envelope): a single line that is
/// bigger than the rest of the room is refused rather than sent, since the core ends a link that sends a frame too big.
const MAX_ANSWER: usize = 900 * 1024;

/// The log's files in `data`, oldest first.
pub fn files(data: &Path) -> Vec<PathBuf> {
    ["connector.log.2", "connector.log.1", "connector.log"]
        .map(|name| data.join(name))
        .to_vec()
}

/// The answer to the core's `logs.read` (`{after?, limit}`): a page, or `{"error":{"key"}}` with the key the core maps
/// to its own status. Reads the files and nothing else, and writes no log line: a viewer would feed itself.
pub fn answer(files: &[PathBuf], params: &Value) -> Value {
    let refused = |key: &str| json!({"error": {"key": key}});
    let limit = match params.get("limit") {
        None => DEFAULT_LIMIT,
        Some(limit) => match limit
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
        {
            Some(limit) => limit as usize,
            None => return refused("logs.request_invalid"),
        },
    };
    let after = match params.get("after") {
        None | Some(Value::Null) => None,
        Some(Value::String(after)) => Some(after.as_str()),
        Some(_) => return refused("logs.request_invalid"),
    };
    match read(files, after, limit) {
        Ok(page) => match serde_json::to_value(&page) {
            Ok(value) if value.to_string().len() <= MAX_ANSWER => value,
            _ => refused("logs.unreadable"),
        },
        Err(Refused::Invalid) => refused("logs.request_invalid"),
        Err(Refused::Expired) => refused("logs.cursor_expired"),
        Err(Refused::Unreadable) => refused("logs.unreadable"),
    }
}

#[cfg(test)]
mod tests;
