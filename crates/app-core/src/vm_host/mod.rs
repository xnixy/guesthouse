//! How a VM starts and stops. Everything that knows how the VM and the daemon are brought up is behind
//! [`VmHost`], as the daemon's start-up is changing. Machines with and without a GPU both go through it.

mod local;

pub use local::{LocalHost, last_words};

use crate::context::Context;
use crate::failure::Result;
use crate::machine::Machine;
use crate::monitor::{Command as Qmp, WATCH_SOCKET};
use crate::vm_args::VmPlan;

/// How a VM stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The guest shut itself down.
    Cleanly,
    /// The guest took too long, so QEMU was told to end. For the guest that's a power cut.
    TurnedOff,
}

pub trait VmHost: Send + Sync {
    /// Starts the VM, with the daemon first if the plan has a GPU, and returns once it answers.
    fn start(&self, plan: &VmPlan, machine: &Machine) -> Result<()>;
    /// Asks the guest to shut down, and waits this long before turning it off.
    fn stop(&self, machine: &Machine, seconds: u64) -> Result<Stopped>;
    /// Ends the daemon and gives back the shared memory, once QEMU has gone.
    fn release(&self) -> Result<()>;
}

/// Presses a key on the watch monitor, which leaves the main one free.
pub fn send_key(context: &Context, key: &str) {
    let command = Qmp::new("send-key", serde_json::json!({"keys": [{"type": "qcode", "data": key}]}));
    context.monitor.ask(&context.run_folder(None).join(WATCH_SOCKET), &command);
}

#[cfg(test)]
pub mod stand_in {
    use super::*;
    use crate::failure::Failure;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct RecordingHost {
        pub calls: Mutex<Vec<String>>,
        pub plans: Mutex<Vec<VmPlan>>,
        pub stop_answer: Mutex<Option<Result<Stopped>>>,
        pub start_failure: Mutex<Option<Failure>>,
    }

    impl RecordingHost {
        fn note(&self, text: String) {
            self.calls.lock().unwrap().push(text);
        }

        pub fn said(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl VmHost for RecordingHost {
        fn start(&self, plan: &VmPlan, _machine: &Machine) -> Result<()> {
            self.note(format!("start {}", if plan.is_gpu() { "gpu" } else { "plain" }));
            self.plans.lock().unwrap().push(plan.clone());
            self.start_failure.lock().unwrap().clone().map_or(Ok(()), Err)
        }

        fn stop(&self, _machine: &Machine, seconds: u64) -> Result<Stopped> {
            self.note(format!("stop {seconds}"));
            self.stop_answer.lock().unwrap().clone().unwrap_or(Ok(Stopped::Cleanly))
        }

        fn release(&self) -> Result<()> {
            self.note("release".to_owned());
            Ok(())
        }
    }
}

#[cfg(test)]
mod drive_tests;
#[cfg(test)]
mod tests;
