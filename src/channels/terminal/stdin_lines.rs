//! Line reader of the chat input thread that reports whether a line was typed
//! ahead: input already waiting when the read began. An approval prompt never
//! takes such a line as its answer (see [`super::prompt_line`]).

use std::io::{self, BufRead as _};

pub(super) struct StdinLines {
    source: Source,
}

enum Source {
    /// The terminal stdin through a reader whose buffer is visible, so
    /// waiting input can be detected before each read.
    #[cfg(unix)]
    Terminal(io::BufReader<std::fs::File>),
    /// Any other stdin: nothing is reported as typed ahead.
    Plain(io::Lines<io::StdinLock<'static>>),
}

impl StdinLines {
    pub(super) fn open(stdin_is_tty: bool) -> Self {
        #[cfg(unix)]
        if stdin_is_tty {
            use std::os::fd::AsFd as _;
            match io::stdin().as_fd().try_clone_to_owned() {
                Ok(fd) => {
                    return Self {
                        source: Source::Terminal(io::BufReader::new(std::fs::File::from(fd))),
                    };
                }
                Err(error) => {
                    tracing::warn!(
                        error = %crate::agent::diagnostics::error_class(&error),
                        "terminal stdin could not be duplicated; typed-ahead lines are not detected"
                    );
                }
            }
        }
        #[cfg(not(unix))]
        let _ = stdin_is_tty;
        Self {
            source: Source::Plain(io::stdin().lock().lines()),
        }
    }

    /// The next line without its line ending, and whether input was already
    /// waiting when the read began. `None` at EOF or on a read error.
    pub(super) fn next_line(&mut self) -> Option<(String, bool)> {
        match &mut self.source {
            #[cfg(unix)]
            Source::Terminal(reader) => {
                let typed_ahead = !reader.buffer().is_empty() || input_waiting(reader.get_ref());
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => {
                        if line.ends_with('\n') {
                            line.pop();
                            if line.ends_with('\r') {
                                line.pop();
                            }
                        }
                        Some((line, typed_ahead))
                    }
                }
            }
            Source::Plain(lines) => match lines.next() {
                Some(Ok(line)) => Some((line, false)),
                Some(Err(_)) | None => None,
            },
        }
    }
}

/// True when the kernel already holds input for `file` (a complete line on a
/// canonical-mode terminal, or EOF). A failed poll reports nothing waiting.
#[cfg(unix)]
fn input_waiting(file: &std::fs::File) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut fds = [PollFd::new(file, PollFlags::IN)];
    let immediately = Timespec { tv_sec: 0, tv_nsec: 0 };
    poll(&mut fds, Some(&immediately)).is_ok_and(|ready| ready > 0)
}
