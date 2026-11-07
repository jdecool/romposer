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

    /// Repository-supplied text with control characters (terminal escapes) removed.
    pub fn clean(text: &str) -> String {
        text.chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect()
    }

    /// Symfony `Terminal::getWidth`: `COLUMNS`, else the controlling terminal, else 80.
    pub fn terminal_width() -> usize {
        if let Ok(cols) = std::env::var("COLUMNS")
            && let Ok(n) = cols.trim().parse::<usize>()
        {
            return n;
        }
        for fd in [1, 0, 2] {
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            // SAFETY: TIOCGWINSZ writes a winsize into the buffer we pass.
            if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 {
                return size.ws_col as usize;
            }
        }
        80
    }

    /// Everything written so far, one entry per line, when buffered.
    pub fn output(&self) -> String {
        self.buffer
            .as_ref()
            .map(|b| b.join("\n") + "\n")
            .unwrap_or_default()
    }
}
