//! What only the Unix mode bits and links can say.

// Tests read the mode bits themselves, which is what they check `platform::fs` against.
#![allow(clippy::disallowed_methods)]

use super::tests::Scratch;
use super::*;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_private_directory_is_refused_when_others_can_enter_or_it_is_a_link() {
    let scratch = Scratch::new("dir");
    let dir = scratch.0.join("data");
    ensure_private_dir(&dir).unwrap();
    assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    // Created again: already there and still private.
    ensure_private_dir(&dir).unwrap();
    chmod(&dir, 0o750);
    assert!(check_private_dir(&dir).is_err());
    assert!(
        ensure_private_dir(&dir).is_err(),
        "an open directory is refused, not repaired"
    );
    assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o750);
    chmod(&dir, 0o700);
    let link = scratch.0.join("link");
    symlink(&dir, &link).unwrap();
    assert!(check_private_dir(&link).is_err());
    assert!(ensure_private_dir(Path::new("relative/dir")).is_err());
}

#[test]
fn an_ancestor_others_can_replace_entries_in_is_refused_unless_sticky() {
    let scratch = Scratch::new("anc");
    let open = scratch.0.join("open");
    fs::create_dir(&open).unwrap();
    chmod(&open, 0o777);
    let inside = open.join("data");
    assert!(ensure_private_dir(&inside).is_err());
    chmod(&open, 0o1777);
    ensure_private_dir(&inside).unwrap();
    verify_ancestors(&inside).unwrap();
}

#[test]
fn a_trusted_record_is_read_only_when_it_is_this_users_regular_file() {
    let scratch = Scratch::new("read");
    let file = scratch.0.join("record.json");
    assert!(read_trusted(&file, 1024).unwrap().is_none());
    write_private(&file, br#"{"a":1}"#).unwrap();
    assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
    assert_eq!(read_trusted(&file, 1024).unwrap().unwrap(), br#"{"a":1}"#);
    assert!(read_trusted(&file, 3).is_err(), "oversized");
    chmod(&file, 0o622);
    assert!(read_trusted(&file, 1024).is_err(), "writable by others");
    chmod(&file, 0o600);
    let link = scratch.0.join("link.json");
    symlink(&file, &link).unwrap();
    assert!(read_trusted(&link, 1024).is_err(), "a link is not followed");
    assert!(
        read_trusted(&scratch.0, 1024).is_err(),
        "not a regular file"
    );
}

#[test]
fn a_private_write_replaces_the_record_whole_and_leaves_no_temporary_file() {
    let scratch = Scratch::new("write");
    let file = scratch.0.join("state.json");
    write_private(&file, b"first").unwrap();
    write_private(&file, b"second").unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"second");
    check_private_file(&file).unwrap();
    let names: Vec<_> = fs::read_dir(&scratch.0)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["state.json"]);
    // Never written into a directory others can enter.
    chmod(&scratch.0, 0o755);
    assert!(write_private(&scratch.0.join("other.json"), b"x").is_err());
    chmod(&scratch.0, 0o700);
}

#[test]
fn a_socket_is_trusted_only_in_a_private_directory_and_open_to_nobody_else() {
    let scratch = Scratch::new("sock");
    let path = scratch.0.join("s.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    chmod(&path, 0o600);
    verify_socket(&path).unwrap();
    chmod(&path, 0o666);
    assert!(verify_socket(&path).is_err());
    chmod(&path, 0o600);
    chmod(&scratch.0, 0o755);
    assert!(verify_socket(&path).is_err());
    chmod(&scratch.0, 0o700);
    let file = scratch.0.join("plain");
    write_private(&file, b"").unwrap();
    assert!(verify_socket(&file).is_err());
}

#[test]
fn an_executable_must_be_a_regular_file_nobody_else_can_change() {
    let scratch = Scratch::new("exe");
    let program = scratch.0.join("program");
    fs::write(&program, b"#!/bin/sh\n").unwrap();
    chmod(&program, 0o700);
    trusted_executable(&program).unwrap();
    chmod(&program, 0o600);
    assert!(trusted_executable(&program).is_err(), "not executable");
    chmod(&program, 0o722);
    assert!(trusted_executable(&program).is_err(), "writable by others");
    chmod(&program, 0o700);
    let link = scratch.0.join("link");
    symlink(&program, &link).unwrap();
    assert!(trusted_executable(&link).is_err());
}
