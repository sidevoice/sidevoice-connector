use super::Ended;
use crate::platform::process::Child;
use std::io;

pub struct Signals;

impl Signals {
    pub fn install() -> io::Result<Signals> {
        Ok(Signals)
    }

    /// The exit code, unchanged: a Windows program ends with a code and nothing else.
    pub async fn supervise(&mut self, child: &mut Child) -> io::Result<Ended> {
        Ok(Ended::Code(child.wait().await?.code().unwrap_or(1)))
    }
}

/// A Windows program has no signal to die of.
pub fn raise(_signal: i32) {}
