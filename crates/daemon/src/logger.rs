//! Log lines to standard error, which the script that starts the daemon sends to a file.

use log::{Level, Log, Metadata, Record};
use std::fmt::Display;
use std::time::{Duration, Instant};

struct StandardError {
    /// When the daemon started, if lines are to say how long after it they come. Only the verbose log
    /// has times, as other output is read by people and scripts that expect it as it is.
    started: Option<Instant>,
}

impl Log for StandardError {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            match self.started {
                Some(started) => {
                    eprintln!("{}", timed_line(started.elapsed(), record.level(), record.args()))
                }
                None => eprintln!("{:<5} {}", record.level(), record.args()),
            }
        }
    }

    fn flush(&self) {}
}

/// A line with the time since the daemon started after its level, so the level stays first. The time
/// is in seconds to the millisecond, and a fixed width keeps the lines aligned.
fn timed_line(elapsed: Duration, level: Level, message: impl Display) -> String {
    format!("{level:<5} [{:>9.3}] {message}", elapsed.as_secs_f64())
}

/// Starts logging. Debug lines cost time on the message path, so they stay off unless asked for.
pub fn start(verbose: bool) {
    let level = if verbose { Level::Debug } else { Level::Info };
    let logger = StandardError { started: verbose.then(Instant::now) };
    if log::set_logger(Box::leak(Box::new(logger))).is_ok() {
        log::set_max_level(level.to_level_filter());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_has_its_level_then_the_time_in_milliseconds() {
        let line = timed_line(Duration::from_millis(123_456), Level::Debug, "wait on gpu");
        assert_eq!(line, "DEBUG [  123.456] wait on gpu");
    }

    #[test]
    fn times_line_up_from_the_start_to_hours_in() {
        let early = timed_line(Duration::from_millis(5_010), Level::Error, "queue");
        assert_eq!(early, "ERROR [    5.010] queue");
        let late = timed_line(Duration::from_secs(36_000), Level::Info, "queue");
        assert_eq!(late, "INFO  [36000.000] queue");
        assert_eq!(early.find(']'), late.find(']'));
    }
}
