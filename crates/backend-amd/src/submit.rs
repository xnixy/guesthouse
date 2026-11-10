//! Submitting a command buffer and waiting for it through an eventfd.
//!
//! `DRM_AMDGPU_CS` takes a context, an indirect buffer at a GPU address, and a set of chunks. This
//! backend sends two: the IB chunk and a syncobj-out chunk, so the kernel signals a sync object when
//! the work retires. There is no doorbell: the submission is an ioctl. The daemon's own runs wait
//! for it on an eventfd armed on that sync object. A guest's run isn't waited for: it ends by
//! writing the guest's fence, see `fence_ib`, and its sync object is only looked at afterwards.
//!
//! A buffer with a GPU address in this file's VM is not kept there for a submission by that alone:
//! only buffers made for the VM itself are, and a space's are taken in from another open. amdgpu can
//! move any other buffer out while the work runs, and the card then faults on its addresses. So a
//! submission names the buffers it may use in a `BO_HANDLES` chunk, which makes amdgpu bring them in
//! first and keep them in until the work retires.

use crate::ioctl::{self, call, get64, put32, put64};
use crate::memory::GemHandle;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// Context op and offsets.
const CTX_OP_ALLOC: u32 = 1;
const CTX_OP_FREE: u32 = 2;
const CTX_IN_OP: usize = 0;
const CTX_IN_CTX_ID: usize = 8;
const CTX_OUT_CTX_ID: usize = 0;
const CTX_REQUEST_SIZE: usize = 16;

// Chunk ids.
const CHUNK_ID_IB: u32 = 0x01;
const CHUNK_ID_SYNCOBJ_OUT: u32 = 0x05;
const CHUNK_ID_BO_HANDLES: u32 = 0x06;

// drm_amdgpu_cs_chunk: id, length in dwords, pointer to the chunk's data.
const CHUNK_ID: usize = 0;
const CHUNK_LENGTH_DW: usize = 4;
const CHUNK_DATA: usize = 8;
const CHUNK_SIZE: usize = 16;

// drm_amdgpu_cs_chunk_ib, 32 bytes.
const IB_VA_START: usize = 8;
const IB_BYTES: usize = 16;
const IB_IP_TYPE: usize = 20;
const IB_IP_INSTANCE: usize = 24;
const IB_RING: usize = 28;
const IB_CHUNK_SIZE: usize = 32;

// drm_amdgpu_bo_list_in, 24 bytes, pointing at drm_amdgpu_bo_list_entry records of 8 bytes: a
// handle and a priority, left at 0.
const BO_LIST_NUMBER: usize = 8;
const BO_LIST_ENTRY_SIZE: usize = 12;
const BO_LIST_ENTRIES: usize = 16;
const BO_LIST_SIZE: usize = 24;
const BO_ENTRY_SIZE: usize = 8;

// drm_amdgpu_cs in/out, 24 bytes.
const CS_CTX_ID: usize = 0;
const CS_BO_LIST: usize = 4;
const CS_NUM_CHUNKS: usize = 8;
const CS_CHUNKS: usize = 16;
const CS_OUT_HANDLE: usize = 0;
const CS_REQUEST_SIZE: usize = 24;

// drm_syncobj_create / _destroy, 8 bytes.
const SYNCOBJ_HANDLE: usize = 0;
const SYNCOBJ_REQUEST_SIZE: usize = 8;

// drm_syncobj_wait, 32 bytes: a pointer to the handles, a time to wait until, how many handles, flags,
// and which one was signalled first. A time of 0 is already past, so the call only looks.
const WAIT_HANDLES: usize = 0;
const WAIT_COUNT: usize = 16;
const WAIT_REQUEST_SIZE: usize = 32;

// drm_syncobj_eventfd, 24 bytes: handle, flags, point (u64), fd (s32), pad.
const EVENTFD_HANDLE: usize = 0;
const EVENTFD_FD: usize = 16;
const EVENTFD_REQUEST_SIZE: usize = 24;

/// An amdgpu submission context. Work is submitted against it, and it reports lost queues.
#[derive(Debug)]
pub struct Context {
    id: u32,
}

impl Context {
    pub fn create(file: &impl AsRawFd) -> io::Result<Self> {
        let mut request = [0u8; CTX_REQUEST_SIZE];
        put32(&mut request, CTX_IN_OP, CTX_OP_ALLOC);
        ioctl::drm_ioctl(file, call::ctx(), &mut request)?;
        Ok(Self { id: ioctl::get32(&request, CTX_OUT_CTX_ID) })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn destroy(&self, file: &impl AsRawFd) {
        let mut request = [0u8; CTX_REQUEST_SIZE];
        put32(&mut request, CTX_IN_OP, CTX_OP_FREE);
        put32(&mut request, CTX_IN_CTX_ID, self.id);
        let _ = ioctl::drm_ioctl(file, call::ctx(), &mut request);
    }
}

/// A DRM sync object. The kernel signals it when the submission that names it retires.
#[derive(Debug)]
pub struct Syncobj {
    handle: u32,
}

impl Syncobj {
    pub fn create(file: &impl AsRawFd) -> io::Result<Self> {
        let mut request = [0u8; SYNCOBJ_REQUEST_SIZE];
        ioctl::drm_ioctl(file, call::syncobj_create(), &mut request)?;
        Ok(Self { handle: ioctl::get32(&request, SYNCOBJ_HANDLE) })
    }

    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Arms an eventfd that becomes readable when this sync object is signalled. The eventfd is the
    /// event source the backend trait does not have today.
    pub fn arm_eventfd(&self, file: &impl AsRawFd) -> io::Result<OwnedFd> {
        // SAFETY: eventfd returns a fresh descriptor or -1.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor is fresh and owned here.
        let event = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut request = [0u8; EVENTFD_REQUEST_SIZE];
        put32(&mut request, EVENTFD_HANDLE, self.handle);
        put32(&mut request, EVENTFD_FD, event.as_raw_fd() as u32);
        ioctl::drm_ioctl(file, call::syncobj_eventfd(), &mut request)?;
        Ok(event)
    }

    /// Whether the kernel has signalled it, without waiting. A submission must have named it.
    pub fn signalled(&self, file: &impl AsRawFd) -> io::Result<bool> {
        let handles = [self.handle];
        let mut request = [0u8; WAIT_REQUEST_SIZE];
        put64(&mut request, WAIT_HANDLES, handles.as_ptr() as u64);
        put32(&mut request, WAIT_COUNT, 1);
        match ioctl::drm_ioctl(file, call::syncobj_wait(), &mut request) {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::ETIME) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn destroy(&self, file: &impl AsRawFd) {
        let mut request = [0u8; SYNCOBJ_REQUEST_SIZE];
        put32(&mut request, SYNCOBJ_HANDLE, self.handle);
        let _ = ioctl::drm_ioctl(file, call::syncobj_destroy(), &mut request);
    }
}

/// Waits for an armed eventfd to become readable, up to a timeout. `true` means the work retired.
pub fn wait_eventfd(event: &impl AsRawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut poll = libc::pollfd { fd: event.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let ready = loop {
        // SAFETY: a single live pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
        if ready >= 0 {
            break ready;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    if ready == 0 {
        return Ok(false);
    }
    let mut counter = [0u8; 8];
    // SAFETY: reading the 8-byte counter an eventfd yields.
    let _ = unsafe { libc::read(event.as_raw_fd(), counter.as_mut_ptr().cast(), counter.len()) };
    Ok(true)
}

/// One indirect buffer: where it is in the file's address space and how many bytes it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ib {
    pub address: u64,
    pub bytes: u32,
}

/// Submits one IB and asks the kernel to signal `signal` when it retires. The IB is already at
/// `ib_va` in this file's address space, `ib_bytes` long, for the engine family `ip_type`. No
/// buffers are named, which suits a probe whose buffers are small and just made.
pub fn submit(
    file: &impl AsRawFd,
    context: &Context,
    ip_type: u32,
    ib_va: u64,
    ib_bytes: u32,
    signal: &Syncobj,
) -> io::Result<u64> {
    submit_all(file, context, ip_type, &[Ib { address: ib_va, bytes: ib_bytes }], &[], signal)
}

/// Submits IBs to run in order on one engine, and asks the kernel to signal `signal` when the last
/// retires. `buffers`, handles on `file`, are kept in place until then.
pub fn submit_all(
    file: &impl AsRawFd,
    context: &Context,
    ip_type: u32,
    ibs: &[Ib],
    buffers: &[GemHandle],
    signal: &Syncobj,
) -> io::Result<u64> {
    let mut ib_chunks = vec![[0u8; IB_CHUNK_SIZE]; ibs.len()];
    for (chunk, ib) in ib_chunks.iter_mut().zip(ibs) {
        put64(chunk, IB_VA_START, ib.address);
        put32(chunk, IB_BYTES, ib.bytes);
        put32(chunk, IB_IP_TYPE, ip_type);
        put32(chunk, IB_IP_INSTANCE, 0);
        put32(chunk, IB_RING, 0);
    }
    let mut sem_chunk = [0u8; 4];
    put32(&mut sem_chunk, 0, signal.handle());

    let entries = bo_entries(buffers);
    let mut bo_list = [0u8; BO_LIST_SIZE];
    put32(&mut bo_list, BO_LIST_NUMBER, buffers.len() as u32);
    put32(&mut bo_list, BO_LIST_ENTRY_SIZE, BO_ENTRY_SIZE as u32);
    put64(&mut bo_list, BO_LIST_ENTRIES, entries.as_ptr() as u64);

    let mut chunks = vec![[0u8; CHUNK_SIZE]; ibs.len() + 1];
    for (slot, ib_chunk) in chunks.iter_mut().zip(&ib_chunks) {
        write_chunk(slot, CHUNK_ID_IB, IB_CHUNK_SIZE / 4, ib_chunk.as_ptr() as u64);
    }
    write_chunk(&mut chunks[ibs.len()], CHUNK_ID_SYNCOBJ_OUT, 1, sem_chunk.as_ptr() as u64);
    if !buffers.is_empty() {
        let mut slot = [0u8; CHUNK_SIZE];
        write_chunk(&mut slot, CHUNK_ID_BO_HANDLES, BO_LIST_SIZE / 4, bo_list.as_ptr() as u64);
        chunks.push(slot);
    }
    let chunk_pointers: Vec<u64> = chunks.iter().map(|chunk| chunk.as_ptr() as u64).collect();

    let mut request = [0u8; CS_REQUEST_SIZE];
    put32(&mut request, CS_CTX_ID, context.id());
    put32(&mut request, CS_BO_LIST, 0);
    put32(&mut request, CS_NUM_CHUNKS, chunk_pointers.len() as u32);
    put64(&mut request, CS_CHUNKS, chunk_pointers.as_ptr() as u64);
    ioctl::drm_ioctl(file, call::cs(), &mut request)?;
    Ok(get64(&request, CS_OUT_HANDLE))
}

/// The list entries for `buffers`, each at the default priority.
fn bo_entries(buffers: &[GemHandle]) -> Vec<u8> {
    let mut entries = vec![0u8; buffers.len() * BO_ENTRY_SIZE];
    for (entry, buffer) in entries.as_chunks_mut::<BO_ENTRY_SIZE>().0.iter_mut().zip(buffers) {
        put32(entry, 0, buffer.0);
    }
    entries
}

fn write_chunk(slot: &mut [u8], id: u32, length_dw: usize, data: u64) {
    put32(slot, CHUNK_ID, id);
    put32(slot, CHUNK_LENGTH_DW, length_dw as u32);
    put64(slot, CHUNK_DATA, data);
}

// SDMA packet building. A write-linear packet copies immediate dwords from the IB into memory at a
// GPU address, which is the smallest thing that proves the engine ran and wrote where it was told.
const SDMA_OPCODE_WRITE_LINEAR: u32 = 0x00000002;
const SDMA_OPCODE_NOP: u32 = 0x00000000;
const SDMA_IB_ALIGNMENT_DWORDS: usize = 8;

/// Builds an SDMA IB that writes `data` to `dst_va`, padded with NOPs to the engine's alignment.
pub fn sdma_write_ib(dst_va: u64, data: &[u32]) -> Vec<u32> {
    let mut ib = Vec::with_capacity(data.len() + 4 + SDMA_IB_ALIGNMENT_DWORDS);
    ib.push(SDMA_OPCODE_WRITE_LINEAR);
    ib.push(dst_va as u32);
    ib.push((dst_va >> 32) as u32);
    ib.push(data.len() as u32 - 1);
    ib.extend_from_slice(data);
    while ib.len() % SDMA_IB_ALIGNMENT_DWORDS != 0 {
        ib.push(SDMA_OPCODE_NOP);
    }
    ib
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_packet_carries_its_address_and_payload() {
        let ib = sdma_write_ib(0x1_2345_6000, &[0xCAFE_D00D]);
        assert_eq!(ib[0], SDMA_OPCODE_WRITE_LINEAR);
        assert_eq!(ib[1], 0x2345_6000);
        assert_eq!(ib[2], 0x1);
        assert_eq!(ib[3], 0, "count is dwords minus one");
        assert_eq!(ib[4], 0xCAFE_D00D);
        assert_eq!(ib.len() % SDMA_IB_ALIGNMENT_DWORDS, 0, "padded to the engine's alignment");
    }

    #[test]
    fn each_buffer_is_listed_by_its_handle_at_the_default_priority() {
        let entries = bo_entries(&[GemHandle(3), GemHandle(9)]);
        assert_eq!(entries, [3, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0]);
    }
}
