//! The steps of a task, read from the log it writes them to. A step that is replaced a moment later
//! is gone from the task's small file but not from here.

use crate::context::Context;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// What a task says when it has done, which is not a step.
const FINISHED: &str = "finished";

pub(super) struct Steps {
    file: PathBuf,
    action: String,
    read_to: u64,
}

impl Steps {
    /// Starts from the end of the log as it is now, so only what a task says after this counts.
    pub(super) fn from_end(context: &Context, action: &str) -> Self {
        let file = context.paths.log_file();
        let read_to = file.metadata().map_or(0, |found| found.len());
        Self { file, action: action.to_owned(), read_to }
    }

    /// The steps of the action written since the last look. Each line of the log is a clock, the
    /// action and the step. A line still being written is left for the next look.
    pub(super) fn new_steps(&mut self) -> Vec<String> {
        let Ok(mut file) = File::open(&self.file) else { return Vec::new() };
        let mut data = Vec::new();
        if file.seek(SeekFrom::Start(self.read_to)).is_err() || file.read_to_end(&mut data).is_err() {
            return Vec::new();
        }
        let whole = data.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
        self.read_to += whole as u64;
        let text = String::from_utf8_lossy(&data[..whole]);
        text.lines().filter_map(|line| self.step_of(line)).collect()
    }

    fn step_of(&self, line: &str) -> Option<String> {
        let (_clock, rest) = line.split_once(' ')?;
        let step = rest.strip_prefix(self.action.as_str())?.strip_prefix(' ')?;
        (step != FINISHED).then(|| step.to_owned())
    }
}
