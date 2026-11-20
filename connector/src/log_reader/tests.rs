//! The reader over real private files, rotated as the connector's writer rotates them. The same list of behaviours as
//! the core's reader (`rust/server/logs/tests.rs` in sidevoice-core); a test added to one is added to the other.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::{read, Page, Refused};
use crate::platform::fs::{Create, Files, Fs};

fn files(log: &Path) -> Vec<PathBuf> {
    ["log.2", "log.1", "log"]
        .map(|extension| log.with_extension(extension))
        .to_vec()
}

fn append(path: &Path, text: &str) {
    let mut file = Files::open_private(path, Create::Append).unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// What the writer does when the log is full: the older generation is dropped, the others move up.
fn rotate(log: &Path) {
    let [second, first] = ["log.2", "log.1"].map(|extension| log.with_extension(extension));
    let _ = fs::remove_file(&second);
    if first.exists() {
        fs::rename(&first, &second).unwrap();
    }
    fs::rename(log, &first).unwrap();
}

fn texts(page: &Page) -> Vec<&str> {
    page.lines.iter().map(|line| line.text.as_str()).collect()
}

/// A log in a private directory that lives as long as the guard.
fn log() -> (TempDir, PathBuf, Vec<PathBuf>) {
    let temp = crate::test_support::temp_dir("log");
    let log = temp.path().join("connector.log");
    let files = files(&log);
    (temp, log, files)
}

fn numbered(from: usize, to: usize) -> String {
    (from..to).map(|n| format!("line {n}\n")).collect()
}

// --- The reader ---------------------------------------------------------------------------------

#[test]
fn an_empty_log_answers_with_the_start_cursor_and_no_line() {
    let (_temp, log, files) = log();
    let page = read(&files, None, 10).unwrap();
    assert!(page.lines.is_empty() && !page.more);
    assert_eq!(page.cursor, "start");
    append(&log, "");
    let page = read(&files, Some("start"), 10).unwrap();
    assert!(page.lines.is_empty());
    assert_eq!(page.cursor, "start");
}

#[test]
fn the_tail_spans_the_rotated_generations_and_ends_at_the_end() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    rotate(&log);
    append(&log, &numbered(3, 5));
    let page = read(&files, None, 4).unwrap();
    assert_eq!(texts(&page), ["line 1", "line 2", "line 3", "line 4"]);
    assert!(!page.more);
    assert_eq!(read(&files, None, 1000).unwrap().lines.len(), 5);
    assert!(read(&files, Some(&page.cursor), 10)
        .unwrap()
        .lines
        .is_empty());
    append(&log, "line 5\n");
    let next = read(&files, Some(&page.cursor), 10).unwrap();
    assert_eq!(texts(&next), ["line 5"]);
}

#[test]
fn the_tail_of_a_big_log_is_whole_lines_within_the_page_size() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 20_000));
    let page = read(&files, None, 1000).unwrap();
    assert_eq!(page.lines.len(), 1000);
    assert_eq!(page.lines[0].text, "line 19000");
    assert_eq!(page.lines[999].text, "line 19999");
    let long = "x".repeat(1000);
    let (_temp, log, files) = self::log();
    append(&log, &format!("{long}\n").repeat(2000));
    let page = read(&files, None, 1000).unwrap();
    assert!(
        (100..150).contains(&page.lines.len()),
        "{}",
        page.lines.len()
    );
    assert!(page.lines.iter().all(|line| line.text == long));
}

#[test]
fn a_page_is_at_most_limit_lines_and_the_cursor_pages_through_without_loss_or_repeat() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 10));
    let mut seen = Vec::new();
    let mut after = Some("start".to_owned());
    let mut pages = 0;
    loop {
        let page = read(&files, after.as_deref(), 4).unwrap();
        assert!(page.lines.len() <= 4);
        seen.extend(page.lines.iter().map(|line| line.text.clone()));
        pages += 1;
        after = Some(page.cursor);
        if !page.more {
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(
        seen,
        (0..10).map(|n| format!("line {n}")).collect::<Vec<_>>()
    );
}

#[test]
fn a_line_has_one_id_however_it_is_read_again() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 6));
    let first = read(&files, Some("start"), 1000).unwrap();
    let tail = read(&files, None, 1000).unwrap();
    let ids: Vec<_> = first.lines.iter().map(|line| &line.id).collect();
    assert_eq!(
        ids,
        tail.lines.iter().map(|line| &line.id).collect::<Vec<_>>()
    );
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 6);
    // It stays when the log rotates under it, and a cursor and an id are the same short, URL-safe thing.
    rotate(&log);
    append(&log, "line 6\n");
    let again = read(&files, Some("start"), 1000).unwrap();
    assert_eq!(
        again.lines[..6]
            .iter()
            .map(|line| &line.id)
            .collect::<Vec<_>>(),
        ids
    );
    for text in again
        .lines
        .iter()
        .map(|line| &line.id)
        .chain([&again.cursor])
    {
        assert!(text.len() <= 128);
        assert!(text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'));
    }
}

#[test]
fn a_cursor_goes_on_through_the_generations_after_a_rotation_without_loss_or_repeat() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    let cursor = read(&files, None, 100).unwrap().cursor;
    append(&log, &numbered(3, 5));
    rotate(&log);
    append(&log, &numbered(5, 8));
    let page = read(&files, Some(&cursor), 100).unwrap();
    assert_eq!(
        texts(&page),
        ["line 3", "line 4", "line 5", "line 6", "line 7"]
    );
    // Once more, with the cursor still in the first generation: now the second older one.
    rotate(&log);
    append(&log, &numbered(8, 9));
    let page = read(&files, Some(&cursor), 100).unwrap();
    assert_eq!(texts(&page).len(), 6);
    assert_eq!(page.lines[0].text, "line 3");
    assert_eq!(page.lines[5].text, "line 8");
    // A page that ends exactly where a generation ends goes on in the next.
    let ends = read(&files, Some(&cursor), 2).unwrap();
    assert_eq!(texts(&ends), ["line 3", "line 4"]);
    assert!(ends.more);
    let rest = read(&files, Some(&ends.cursor), 100).unwrap();
    assert_eq!(texts(&rest), ["line 5", "line 6", "line 7", "line 8"]);
    assert!(!rest.more);
}

#[test]
fn a_file_seen_under_two_names_is_one_generation_and_a_log_between_rotations_is_continuous() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    // A rotation between two opens shows one file twice.
    let twice = [log.clone(), log.clone()];
    assert_eq!(texts(&read(&twice, Some("start"), 10).unwrap()).len(), 3);
    // Renamed, and the new log not yet there.
    rotate(&log);
    let page = read(&files, None, 10).unwrap();
    assert_eq!(texts(&page), ["line 0", "line 1", "line 2"]);
    assert!(read(&files, Some(&page.cursor), 10)
        .unwrap()
        .lines
        .is_empty());
    append(&log, "line 3\n");
    let next = read(&files, Some(&page.cursor), 10).unwrap();
    assert_eq!(texts(&next), ["line 3"]);
}

#[test]
fn a_cursor_whose_generation_was_deleted_is_expired() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    let cursor = read(&files, None, 100).unwrap().cursor;
    for round in 0..3 {
        rotate(&log);
        append(&log, &numbered(10 * (round + 1), 10 * (round + 1) + 2));
        let held = read(&files, Some(&cursor), 100);
        assert_eq!(held.is_ok(), round < 2, "after {} rotations", round + 1);
    }
    assert_eq!(
        read(&files, Some(&cursor), 100).err(),
        Some(Refused::Expired)
    );
    // And one that was never there.
    assert_eq!(
        read(&files, Some("0123456789abcdef.0"), 100).err(),
        Some(Refused::Expired)
    );
}

#[test]
fn a_cursor_past_the_end_of_its_file_is_expired() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    let (id, offset) = read(&files, None, 100)
        .unwrap()
        .cursor
        .split_once('.')
        .map(|(i, o)| (i.to_owned(), o.parse::<u64>().unwrap()))
        .unwrap();
    for past in [offset + 1, offset + 100] {
        assert_eq!(
            read(&files, Some(&format!("{id}.{past}")), 10).err(),
            Some(Refused::Expired)
        );
    }
    // Where it ends, and where it starts, are fine.
    assert!(read(&files, Some(&format!("{id}.{offset}")), 10)
        .unwrap()
        .lines
        .is_empty());
    assert_eq!(
        read(&files, Some(&format!("{id}.0")), 10)
            .unwrap()
            .lines
            .len(),
        3
    );
}

#[test]
fn a_malformed_cursor_is_invalid() {
    let (_temp, log, files) = log();
    append(&log, "alpha\nbeta\n");
    let valid = read(&files, None, 10).unwrap().cursor;
    let id = valid.split_once('.').unwrap().0;
    let too_long = format!("{id}.{}", "1".repeat(130));
    for bad in [
        "",
        "x",
        "start.",
        "start0",
        ".",
        "0123456789abcdef",
        "0123456789abcdef.",
        "0123456789abcde.1",
        "0123456789abcdefg.1",
        "zzzzzzzzzzzzzzzz.1",
        "0123456789abcdef.-1",
        "0123456789abcdef.+1",
        "0123456789abcdef.1.2",
        "0123456789abcdef.1x",
        "../../etc/passwd",
        &too_long,
        // Right generation, but in the middle of a line.
        &format!("{id}.3"),
    ] {
        assert_eq!(
            read(&files, Some(bad), 10).err(),
            Some(Refused::Invalid),
            "{bad:?}"
        );
    }
}

#[test]
fn a_torn_last_line_is_never_delivered_and_is_whole_when_it_is_finished() {
    let (_temp, log, files) = log();
    append(&log, "alpha\nbeta\ngam");
    let page = read(&files, None, 10).unwrap();
    assert_eq!(texts(&page), ["alpha", "beta"]);
    assert_eq!(
        texts(&read(&files, Some("start"), 10).unwrap()),
        ["alpha", "beta"]
    );
    assert!(read(&files, Some(&page.cursor), 10)
        .unwrap()
        .lines
        .is_empty());
    append(&log, "ma\ndelta\n");
    let next = read(&files, Some(&page.cursor), 10).unwrap();
    assert_eq!(texts(&next), ["gamma", "delta"]);
    // A generation that is only a torn line has nothing to give yet.
    let (_temp, log, files) = self::log();
    append(&log, "never ended");
    let page = read(&files, None, 10).unwrap();
    assert!(page.lines.is_empty());
    assert_eq!(page.cursor, "start");
}

#[test]
fn a_page_stops_at_its_size_but_always_holds_a_whole_line() {
    let (_temp, log, files) = log();
    let big = "b".repeat(60_000);
    append(&log, &format!("{big}\n").repeat(5));
    let mut sizes = Vec::new();
    let mut after = "start".to_owned();
    loop {
        let page = read(&files, Some(&after), 1000).unwrap();
        sizes.push(page.lines.len());
        after = page.cursor;
        if !page.more {
            break;
        }
    }
    assert_eq!(sizes, [2, 2, 1]);
    let (_temp, log, files) = self::log();
    let huge = "h".repeat(300_000);
    append(&log, &format!("{huge}\nsmall\n"));
    let page = read(&files, Some("start"), 1000).unwrap();
    assert_eq!(texts(&page), [huge.as_str()]);
    assert!(page.more);
    let tail = read(&files, None, 1000).unwrap();
    assert_eq!(texts(&tail), ["small"]);
}

#[test]
fn a_line_is_its_text_without_the_line_ending() {
    let (_temp, log, files) = log();
    append(&log, "one\r\ntwo\n\nlast words \n");
    let page = read(&files, None, 10).unwrap();
    assert_eq!(texts(&page), ["one", "two", "", "last words "]);
}
/// The connector rotates by copy-and-truncate, never by rename ([`crate::logfile::rotate`]): a cursor in the file that
/// was copied aside goes on in the copy and then in the new file, without loss or repeat, and a read between the copy
/// and the truncate sees the generation once.
#[test]
fn a_cursor_survives_the_copy_and_truncate_the_connector_rotates_by() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 50));
    let first = read(&files, Some("start"), 20).unwrap();
    assert_eq!(first.lines.len(), 20);
    assert!(crate::logfile::rotate(&log, 16));
    append(&log, &numbered(50, 60));
    let mut seen: Vec<String> = texts(&first)
        .iter()
        .map(|text| (*text).to_owned())
        .collect();
    let mut after = first.cursor;
    loop {
        let page = read(&files, Some(&after), 7).unwrap();
        seen.extend(page.lines.iter().map(|line| line.text.clone()));
        after = page.cursor;
        if !page.more {
            break;
        }
    }
    let expected: Vec<String> = (20..60).map(|n| format!("line {n}")).collect();
    assert_eq!(seen[20..], expected[..]);
    assert_eq!(seen.len(), 60);
    // Mid-rotation: the copy is there and the file has not been truncated yet.
    let (_temp, log, files) = self::log();
    append(&log, &numbered(0, 5));
    // A copy made as the writer makes it (private); a plain `fs::copy` is not private on Windows.
    append(
        &log.with_extension("log.1"),
        &fs::read_to_string(&log).unwrap(),
    );
    let page = read(&files, None, 100).unwrap();
    assert_eq!(page.lines.len(), 5, "one generation, not two");
    // A rotation in a later generation that was already read through.
    assert!(crate::logfile::rotate(&log, 4));
    let page = read(&files, Some(&page.cursor), 100).unwrap();
    assert!(page.lines.is_empty());
}
