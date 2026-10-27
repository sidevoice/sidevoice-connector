//! Unix: a running program can be unlinked, so no process holds an installed file in a way that stops its removal.

use super::{Holder, Holding};
use std::path::Path;

pub struct Os;

impl Holding for Os {
    fn of(_: &Path) -> Vec<Holder> {
        Vec::new()
    }
}
