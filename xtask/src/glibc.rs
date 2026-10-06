//! The oldest C library a Linux binary runs on.
//!
//! `dist` builds against [`FLOOR`] (`cargo zigbuild --target <triple>.<FLOOR>`) and records it in the inventory;
//! `verify` reads the GLIBC symbol versions the binary needs and refuses one newer than the inventory's floor;
//! `verify-floor` runs the binary in [`FLOOR_IMAGE`], a distribution whose C library is exactly the floor.

use std::cmp::Ordering;

use crate::util::output;
use crate::Result;

/// The glibc release every Linux binary runs on, and every newer one. 2.28: Debian 10, Ubuntu 20.04 (2.31), RHEL
/// and AlmaLinux 8, Amazon Linux 2023 and every later release of each.
pub(crate) const FLOOR: &str = "2.28";

/// A distribution whose C library is glibc [`FLOOR`]: AlmaLinux 8, pinned by its multi-architecture index.
pub(crate) const FLOOR_IMAGE: &str =
    "almalinux:8@sha256:8b469a3a78515e8a18ea8fc727e6a3679e1d0c5ba6f58d5b30be3d0d9b86cffe";

/// "2.28" as (2, 28, 0), so versions compare by number, not by text ("2.9" is older than "2.28").
pub(crate) fn parse(version: &str) -> Option<Vec<u32>> {
    let parts: Option<Vec<u32>> = version.split('.').map(|part| part.parse().ok()).collect();
    parts.filter(|parts| (2..=3).contains(&parts.len()))
}

fn compare(left: &str, right: &str) -> Ordering {
    let pad = |version: &str| {
        let mut parts = parse(version).unwrap_or_default();
        parts.resize(3, 0);
        parts
    };
    pad(left).cmp(&pad(right))
}

/// The newest `GLIBC_x.y[.z]` version a `readelf --version-info` listing requires, if any.
fn newest_in(listing: &str) -> Option<String> {
    listing
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter_map(|word| word.strip_prefix("GLIBC_"))
        .filter(|version| parse(version).is_some())
        .max_by(|left, right| compare(left, right))
        .map(str::to_string)
}

/// The newest glibc version the binary needs a symbol of, from its version requirements (`readelf`).
pub(crate) fn needed(binary: &str) -> Result<String> {
    let listing = output("readelf", &["--version-info", "--wide", binary], None)?;
    newest_in(&listing)
        .ok_or_else(|| format!("{binary} needs no GLIBC symbol version: not a glibc binary"))
}

/// Refuses a binary that needs a glibc newer than `floor`.
pub(crate) fn check(needed: &str, floor: &str) -> Result<()> {
    if compare(needed, floor) == Ordering::Greater {
        return Err(format!(
            "the binary needs glibc {needed}, newer than its floor {floor}: it would not start on older systems"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "Version needs section '.gnu.version_r' contains 2 entries:
 Addr: 0x0000000000000a58  Offset: 0x000a58  Link: 6 (.dynstr)
  000000: Version: 1  File: libgcc_s.so.1  Cnt: 1
  0x0010:   Name: GCC_3.0  Flags: none  Version: 9
  0x0020: Version: 1  File: libc.so.6  Cnt: 4
  0x0030:   Name: GLIBC_2.9  Flags: none  Version: 8
  0x0040:   Name: GLIBC_2.28  Flags: none  Version: 7
  0x0050:   Name: GLIBC_2.3.4  Flags: none  Version: 6
  0x0060:   Name: GLIBC_PRIVATE  Flags: none  Version: 5
";

    #[test]
    fn the_newest_version_is_compared_by_number() {
        assert_eq!(newest_in(LISTING).as_deref(), Some("2.28"));
        assert_eq!(newest_in("Name: GCC_3.0"), None);
    }

    #[test]
    fn a_binary_may_need_the_floor_but_nothing_newer() {
        assert!(check("2.28", FLOOR).is_ok());
        assert!(check("2.17", FLOOR).is_ok());
        assert!(check("2.3.4", FLOOR).is_ok());
        assert!(check("2.29", FLOOR).is_err());
        assert!(check("2.39", FLOOR).is_err());
    }

    #[test]
    fn versions_have_two_or_three_numbers() {
        assert_eq!(parse("2.28"), Some(vec![2, 28]));
        assert_eq!(parse("2.3.4"), Some(vec![2, 3, 4]));
        assert_eq!(parse("2"), None);
        assert_eq!(parse("PRIVATE"), None);
        assert_eq!(parse("2.x"), None);
    }
}
