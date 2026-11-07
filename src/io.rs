//! Console output with Composer's split: `write` to stdout, `writeError` to stderr.

use std::io::Write;

#[derive(Debug, Default)]
pub struct Io {
    pub quiet: bool,
    pub verbosity: u8,
}

impl Io {
    pub fn write(&mut self, line: &str) {
        if !self.quiet {
            let _ = writeln!(std::io::stdout(), "{line}");
        }
    }

    pub fn error(&mut self, line: &str) {
        if !self.quiet {
            let _ = writeln!(std::io::stderr(), "{line}");
        }
    }
}
