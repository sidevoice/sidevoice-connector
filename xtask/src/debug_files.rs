//! The debug files of a release build, kept beside the archive (`target/dist/debug`) and never in it.
//!
//! The shipped binary carries no debug sections (the archive is as small as before), and what Sentry needs to name the frames
//! of a panic (rust/crash.rs: the frames' addresses and the debug id of the module that holds them) is split off here, by
//! `dist`, from the binary as built. `profile.release` keeps line tables and does not strip (`debug = "line-tables-only"`,
//! `strip = "none"`): a build that strips, or on MSVC links without `/DEBUG`, leaves nothing to upload.
//!
//! - Linux: the binary as built, unstripped, is the debug file (line tables, the symbol table and the build id the link adds,
//!   `.cargo/config.toml`); the staged binary is `objcopy --strip-debug` (its symbol table stays: function names). Both are
//!   checked: the debug file must have `.debug_line` and a build id, the shipped binary no `.debug_line` and the same id.
//! - macOS: `dsymutil` makes the `.dSYM` from the binary as built, then the staged binary is `strip -S` and signed again (ad hoc).
//! - Windows: the linker's PDB (the binary only names it by its file name).
//!
//! `cargo xtask sentry-upload` uploads what is in [`dir`].

use std::fs;
use std::path::{Path, PathBuf};

use object::read::elf::ElfFile64;
use object::Object;
use sidevoice_platform::target::ReleaseTarget;

use crate::dist::DIST_DIR;
use crate::util::*;
use crate::Result;

/// Where the debug files are left.
pub(crate) fn dir() -> PathBuf {
    repo().join(DIST_DIR).join("debug")
}

/// Splits the debug files off `built`, the binary as the build made it, into [`dir`], and strips `staged`, the copy that goes
/// into the archive, of its debug sections.
pub(crate) fn split(target: &ReleaseTarget, built: &Path, staged: &Path) -> Result<()> {
    let out = dir();
    let _ = fs::remove_dir_all(&out);
    mkdir(&out)?;
    match target.os {
        "linux" => linux(built, staged, &out),
        "macos" => macos(built, staged, &out),
        _ => windows(built, &out),
    }
}

/// Runs `program` with `args`; its output is not wanted.
fn run(program: &str, args: &[&str]) -> Result<()> {
    output(program, args, None).map(|_| ())
}

fn text(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| "path".to_owned())
}

/// Whether an ELF file has line tables, and its build id.
fn elf_facts(path: &Path) -> Result<(bool, Option<Vec<u8>>)> {
    let data = read(path)?;
    let file = ElfFile64::<object::Endianness>::parse(data.as_slice())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let lines = file.section_by_name(".debug_line").is_some();
    let id = file
        .build_id()
        .map_err(|error| format!("{}: {error}", path.display()))?
        .map(<[u8]>::to_vec);
    Ok((lines, id))
}

fn linux(built: &Path, staged: &Path, out: &Path) -> Result<()> {
    let debug = out.join("sidevoice-connector.debug");
    fs::copy(built, &debug).map_err(|error| format!("{}: {error}", built.display()))?;
    let (lines, id) = elf_facts(&debug)?;
    if !lines {
        return Err("the release binary has no line tables: profile.release is `debug = \"line-tables-only\"`, `strip = \"none\"`".into());
    }
    let Some(id) = id else {
        return Err("the release binary has no build id: .cargo/config.toml links Linux with `--build-id=sha1`".into());
    };
    // `objcopy IN OUT`, which GNU's and LLVM's both take.
    let stripped = staged.with_extension("stripped");
    run(
        "objcopy",
        &["--strip-debug", text(staged)?, text(&stripped)?],
    )?;
    fs::rename(&stripped, staged).map_err(|error| error.to_string())?;
    let (lines, shipped) = elf_facts(staged)?;
    if lines || shipped.as_ref() != Some(&id) {
        return Err(
            "the shipped binary must have no line tables and the debug file's build id".into(),
        );
    }
    Ok(())
}

fn macos(built: &Path, staged: &Path, out: &Path) -> Result<()> {
    let dsym = out.join("sidevoice-connector.dSYM");
    run("dsymutil", &[text(built)?, "-o", text(&dsym)?])?;
    if !dsym.is_dir() {
        return Err("dsymutil made no .dSYM".into());
    }
    run("strip", &["-S", text(staged)?])?;
    // Stripping a Mach-O invalidates the ad-hoc signature the linker put on it, and arm64 refuses to run a binary whose signature
    // is not valid.
    run("codesign", &["--force", "--sign", "-", text(staged)?])
}

fn windows(built: &Path, out: &Path) -> Result<()> {
    // The PDB is named after the crate, with an underscore.
    let pdb = built.with_file_name("sidevoice_connector.pdb");
    fs::copy(&pdb, out.join("sidevoice_connector.pdb"))
        .map(|_| ())
        .map_err(|error| {
            format!(
                "{}: {error} (a build that strips has no PDB: profile.release is `strip = \"none\"`)",
                pdb.display()
            )
        })
}

/// What is in [`dir`], for the upload: a file, or the `.dSYM` directory.
pub(crate) fn files() -> Result<Vec<PathBuf>> {
    let dir = dir();
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        found.push(entry.map_err(|error| error.to_string())?.path());
    }
    found.sort();
    if found.is_empty() {
        return Err(format!("no debug files in {}", dir.display()));
    }
    Ok(found)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The test binary itself is an ELF with line tables and, with the link flags of `.cargo/config.toml`, a build id.
    #[test]
    fn the_debug_file_keeps_the_line_tables_and_the_shipped_binary_loses_them() {
        let work = TempDir::new("sidevoice-debug-files").unwrap();
        let built = work.0.join("built");
        let staged = work.0.join("staged");
        let out = work.0.join("out");
        let exe = std::env::current_exe().unwrap();
        fs::copy(&exe, &built).unwrap();
        fs::copy(&exe, &staged).unwrap();
        mkdir(&out).unwrap();
        linux(&built, &staged, &out).unwrap();
        let (lines, id) = elf_facts(&out.join("sidevoice-connector.debug")).unwrap();
        assert!(lines && id.is_some());
        let (lines, shipped) = elf_facts(&staged).unwrap();
        assert!(!lines, "the shipped binary has no line tables");
        assert_eq!(shipped, id, "and the same build id");
        assert!(
            fs::metadata(&staged).unwrap().len() < fs::metadata(&built).unwrap().len(),
            "smaller"
        );
        // A binary that was stripped before has nothing to upload: refused, not uploaded empty.
        let again = work.0.join("again");
        fs::copy(&staged, &again).unwrap();
        assert!(linux(&again, &staged, &out).is_err());
    }
}
