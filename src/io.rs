//! Console output with Composer's split: `write` to stdout, `writeError` to stderr.
//! A buffered Io keeps the lines instead (the fixture runner's `BufferIO`).

use std::io::Write;

#[derive(Debug, Default)]
pub struct Io {
    pub quiet: bool,
    pub verbosity: u8,
    buffer: Option<Vec<String>>,
}

impl Io {
    pub fn new(quiet: bool, verbosity: u8) -> Io {
        Io {
            quiet,
            verbosity,
            buffer: None,
        }
    }

    /// Collects every line instead of printing it.
    pub fn buffered() -> Io {
        Io {
            quiet: false,
            verbosity: 0,
            buffer: Some(Vec::new()),
        }
    }

    pub fn write(&mut self, line: &str) {
        if let Some(buffer) = &mut self.buffer {
            buffer.push(line.to_string());
        } else if !self.quiet {
            let _ = writeln!(std::io::stdout(), "{line}");
        }
    }

    pub fn error(&mut self, line: &str) {
        if let Some(buffer) = &mut self.buffer {
            buffer.push(line.to_string());
        } else if !self.quiet {
            let _ = writeln!(std::io::stderr(), "{line}");
        }
    }

    /// `<warning>` messages: tags stay visible in buffered output, as Composer's BufferIO
    /// (plain OutputFormatter, no Composer styles) leaves them in the fixtures' expected text.
    pub fn warning(&mut self, line: &str) {
        if self.buffer.is_some() {
            self.error(&format!("<warning>{line}</warning>"));
        } else {
            self.error(line);
        }
    }

    /// Everything written so far, one entry per line, when buffered.
    pub fn output(&self) -> String {
        self.buffer
            .as_ref()
            .map(|b| b.join("\n") + "\n")
            .unwrap_or_default()
    }
}
