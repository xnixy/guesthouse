//! computecore.dll: a compute system's create, start, change and stop, and the files a machine is
//! given leave to open.
//!
//! Every call here waits for HCS to finish. A failed call comes back with HCS's own document, which
//! names the device or setting it refused.

use crate::error::{Error, Result, check};
use crate::schema::{self, ModifySettingRequest, Properties, PropertyQuery};
use crate::text::{take_local, wide};
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::System::HostComputeSystem::{
    HCS_OPERATION, HCS_SYSTEM, HcsCloseComputeSystem, HcsCloseOperation, HcsCreateComputeSystem,
    HcsCreateEmptyGuestStateFile, HcsCreateEmptyRuntimeStateFile, HcsCreateOperation,
    HcsGetComputeSystemProperties, HcsGrantVmAccess, HcsModifyComputeSystem, HcsOpenComputeSystem,
    HcsShutDownComputeSystem, HcsStartComputeSystem, HcsTerminateComputeSystem, HcsWaitForOperationResult,
};

/// HCS's word for waiting as long as it takes.
const WAIT_FOREVER: u32 = u32::MAX;
/// How long a question about a machine's state may take. One that's asked over and over, as a
/// running machine's is, must come back even when HCS doesn't answer, so its asker can go on.
const STATE_WAIT_MS: u32 = 5000;

/// One call's handle, which HCS fills with the call's result.
struct Operation(HCS_OPERATION);

impl Operation {
    fn new(doing: &str) -> Result<Self> {
        // SAFETY: no context and no callback, so HCS keeps nothing of ours.
        let operation = unsafe { HcsCreateOperation(null(), None) };
        if operation.is_null() { Err(Error::last(doing)) } else { Ok(Operation(operation)) }
    }

    /// Waits for the call that was started with this operation, given what starting it returned.
    /// The document HCS answers with, if any, comes back either way.
    fn finish(self, started: i32, doing: &str, wait_ms: u32) -> Result<Option<String>> {
        check(started, doing)?;
        let mut answer = null_mut();
        // SAFETY: the operation is ours and open, and `answer` is a place for HCS's string.
        let code = unsafe { HcsWaitForOperationResult(self.0, wait_ms, &mut answer) };
        // SAFETY: HCS allocates its answer with LocalAlloc for the caller to free.
        let answer = unsafe { take_local(answer) };
        if code < 0 { Err(Error::new(doing, code, answer)) } else { Ok(answer) }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        // SAFETY: the operation is ours, and nothing uses it after this.
        unsafe { HcsCloseOperation(self.0) };
    }
}

/// A machine HCS knows of. Dropping it closes the handle, which stops the machine if its document
/// asked for that and this was the last handle.
pub struct ComputeSystem {
    handle: HCS_SYSTEM,
    id: String,
}

impl ComputeSystem {
    pub fn create(id: &str, document: &schema::ComputeSystem) -> Result<Self> {
        let doing = format!("Creating the machine {id}");
        let json = serde_json::to_string(document).expect("a machine document always serialises");
        let operation = Operation::new(&doing)?;
        let (id_text, json) = (wide(id), wide(&json));
        let mut handle = null_mut();
        // SAFETY: both strings end in a zero and outlive the call, no security descriptor means
        // HCS's default, and `handle` is a place for the machine's handle.
        let started = unsafe {
            HcsCreateComputeSystem(id_text.as_ptr(), json.as_ptr(), operation.0, null(), &mut handle)
        };
        // Built before the wait, so the handle is closed even if creating fails after HCS gave one.
        let system = (!handle.is_null()).then(|| ComputeSystem { handle, id: id.to_owned() });
        operation.finish(started, &doing, WAIT_FOREVER)?;
        system.ok_or_else(|| Error::new(doing, -1, Some("HCS gave no handle".into())))
    }

    /// The machine called `id`, if HCS has one by that name.
    pub fn open(id: &str) -> Option<Self> {
        let id_text = wide(id);
        let mut handle = null_mut();
        // SAFETY: the name ends in a zero, and `handle` is a place for the machine's handle.
        let code = unsafe { HcsOpenComputeSystem(id_text.as_ptr(), GENERIC_ALL, &mut handle) };
        (code >= 0 && !handle.is_null()).then(|| ComputeSystem { handle, id: id.to_owned() })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn start(&self) -> Result<()> {
        self.call("Starting", |handle, operation| {
            // SAFETY: the handle and the operation are open, and no options means HCS's own.
            unsafe { HcsStartComputeSystem(handle, operation, null()) }
        })
        .map(drop)
    }

    /// Asks the guest to shut down, which needs its shutdown service to be running.
    pub fn shut_down(&self) -> Result<()> {
        self.call("Shutting down", |handle, operation| {
            // SAFETY: as in `start`.
            unsafe { HcsShutDownComputeSystem(handle, operation, null()) }
        })
        .map(drop)
    }

    /// Stops the machine at once, as pulling its plug would.
    pub fn terminate(&self) -> Result<()> {
        self.call("Stopping", |handle, operation| {
            // SAFETY: as in `start`.
            unsafe { HcsTerminateComputeSystem(handle, operation, null()) }
        })
        .map(drop)
    }

    pub fn modify(&self, request: &ModifySettingRequest) -> Result<()> {
        let json = wide(serde_json::to_string(request).expect("a change always serialises"));
        self.call(&format!("Changing {}", request.resource_path), |handle, operation| {
            // SAFETY: the change ends in a zero and outlives the call, and no identity handle
            // means the caller's own.
            unsafe { HcsModifyComputeSystem(handle, operation, json.as_ptr(), null_mut()) }
        })
        .map(drop)
    }

    pub fn properties(&self) -> Result<Properties> {
        let query = wide(serde_json::to_string(&PropertyQuery::basic()).expect("a query always serialises"));
        let answer = self.call_waiting("Reading the state of", STATE_WAIT_MS, |handle, operation| {
            // SAFETY: the query ends in a zero and outlives the call.
            unsafe { HcsGetComputeSystemProperties(handle, operation, query.as_ptr()) }
        })?;
        let answer = answer.unwrap_or_default();
        let mut properties: Properties = serde_json::from_str(&answer).map_err(|error| {
            Error::new(format!("Reading the state of {}", self.id), -1, Some(format!("{error}: {answer}")))
        })?;
        properties.raw = answer;
        Ok(properties)
    }

    fn call(
        &self,
        doing: &str,
        start: impl FnOnce(HCS_SYSTEM, HCS_OPERATION) -> i32,
    ) -> Result<Option<String>> {
        self.call_waiting(doing, WAIT_FOREVER, start)
    }

    /// As `call`, giving up after `wait_ms`. Closing the operation then cancels the call.
    fn call_waiting(
        &self,
        doing: &str,
        wait_ms: u32,
        start: impl FnOnce(HCS_SYSTEM, HCS_OPERATION) -> i32,
    ) -> Result<Option<String>> {
        let doing = format!("{doing} the machine {}", self.id);
        let operation = Operation::new(&doing)?;
        let started = start(self.handle, operation.0);
        operation.finish(started, &doing, wait_ms)
    }
}

impl Drop for ComputeSystem {
    fn drop(&mut self) {
        // SAFETY: the handle is ours, and nothing uses it after this.
        unsafe { HcsCloseComputeSystem(self.handle) };
    }
}

/// Lets the machine called `id` open `path`. A machine's worker process runs as the machine's own
/// account, which can't read a user's files until it's let in.
pub fn grant_vm_access(id: &str, path: &Path) -> Result<()> {
    let (id_text, path_text) = (wide(id), wide(path));
    // SAFETY: both strings end in a zero and outlive the call.
    let code = unsafe { HcsGrantVmAccess(id_text.as_ptr(), path_text.as_ptr()) };
    check(code, &format!("Letting the machine {id} open {}", path.display()))
}

/// Makes the file that keeps a machine's firmware settings and TPM, if there isn't one yet.
pub fn ensure_guest_state_file(path: &Path) -> Result<()> {
    ensure_state_file(path, "guest state", |text| {
        // SAFETY: the path ends in a zero and outlives the call.
        unsafe { HcsCreateEmptyGuestStateFile(text) }
    })
}

/// Makes the file a machine's saved state goes in, if there isn't one yet.
pub fn ensure_runtime_state_file(path: &Path) -> Result<()> {
    ensure_state_file(path, "runtime state", |text| {
        // SAFETY: as above.
        unsafe { HcsCreateEmptyRuntimeStateFile(text) }
    })
}

#[allow(clippy::disallowed_methods)] // the host's own files
fn ensure_state_file(path: &Path, kind: &str, create: impl FnOnce(*const u16) -> i32) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    let text = wide(path);
    check(create(text.as_ptr()), &format!("Making the {kind} file {}", path.display()))
}
