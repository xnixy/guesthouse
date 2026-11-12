//! The objects this backend keeps in the RM: one client, one device, and what hangs from them.

use super::classes::{
    CONTROL_GET_GPU_ID, CONTROL_GET_TIME, DEVICE, MEMORY_LOCAL_USER, MEMORY_VIRTUAL, SUBDEVICE,
    VIRTUAL_ADDRESS_SPACE,
};
use super::instance::device_instance_at;
use super::listing::CardPlace;
use super::{DmaMap, Handle, Rm, RmError};
use crate::guest_clock::GuestClock;
use crate::profile::{GuestControl, HostBranch};
use guesthouse_backend::{MemoryKind, PciAddress};
use std::os::fd::OwnedFd;

const DEVICE_HANDLE: Handle = 0xC000_0001;
const SUBDEVICE_HANDLE: Handle = 0xC000_0002;
const FIRST_OBJECT: Handle = 0xC000_1000;

/// Which of the host's GPUs the device is: the RM's device instance, which isn't the device file's
/// number. See [`super::instance`].
const DEVICE_PARAM_INSTANCE: usize = 0x00;
/// Each channel gets its own address space from this client, which needs this mode on the device.
const VA_MODE_MULTIPLE: u32 = 2;
const DEVICE_PARAM_VA_MODE: usize = 0x30;

/// A tag the RM keeps with memory. Any value but zero does.
const MEMORY_OWNER: u32 = 0x6770_7576;
const MEMORY_PARAM_OWNER: usize = 0x00;
const MEMORY_PARAM_TYPE: usize = 0x04;
const MEMORY_PARAM_FLAGS: usize = 0x08;
const MEMORY_PARAM_ATTRIBUTES: usize = 0x18;
const MEMORY_PARAM_SIZE: usize = 0x40;
const MEMORY_PARAM_OFFSET: usize = 0x50;
const MEMORY_PARAM_ADDRESS_SPACE: usize = 0x6C;
const MEMORY_FLAG_HANDLE_PROVIDED: u32 = 0x4000;
/// Memory for a depth surface, which the RM gives the depth kind its attributes' packing calls for.
const MEMORY_TYPE_DEPTH: u32 = 1;
/// Virtual, at a fixed address, with the handle provided, and lazily. Without lazily the page tables
/// for the whole range are made up front, which takes seconds, and every process needs its own.
const VIRTUAL_RANGE_FLAGS: u32 = 0x0008_0000 | 0x0000_0010 | 0x0000_4000 | 0x0000_0400;
const ADDRESS_SPACE_PARAM_SIZE: usize = 0x08;
const ADDRESS_SPACE_SIZE: u64 = 1 << 40;
/// The kernel takes a size that isn't whole pages for a bug of its own.
const EXPORT_GRAIN: u64 = 0x1000;

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A GPU address space, and the ranges reserved in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressSpace {
    pub handle: Handle,
    /// (start, end, the range's object).
    pub ranges: Vec<(u64, u64, Handle)>,
}

impl AddressSpace {
    pub fn range_at(&self, address: u64) -> Result<Handle, RmError> {
        let found = self.ranges.iter().find(|(start, end, _)| (*start..*end).contains(&address));
        found.map(|range| range.2).ok_or_else(|| RmError::System(format!("no range holds {address:#x}")))
    }
}

/// A mapping to make, at a fixed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mapping {
    pub memory: Handle,
    pub offset: u64,
    pub length: u64,
    pub address: u64,
    pub small_pages: bool,
    pub gpu_cached: bool,
    /// The page kind for this mapping alone, if it needs one the memory doesn't have.
    pub page_kind: Option<u32>,
}

/// The client, and the handles it hands out.
pub struct Device<R> {
    pub rm: R,
    /// How long a graphics channel group may hold the GPU. 0 leaves the RM's own choice.
    pub graphics_timeslice_micros: u64,
    pub client: Handle,
    pub device: Handle,
    pub subdevice: Handle,
    /// The RM's device instance of this GPU.
    pub instance: u32,
    /// One doorbell page serves every channel.
    pub doorbell_mapped: bool,
    pub(crate) guest_clock: GuestClock,
    next_handle: Handle,
    sizes: &'static HostBranch,
}

impl<R: Rm> Device<R> {
    /// Makes the client, device and subdevice that everything else hangs from, on the RM's first
    /// device. That's only sure to be the card whose device file is open on a PC with one card.
    pub fn open(mut rm: R, sizes: &'static HostBranch) -> Result<Self, RmError> {
        let client = rm.new_client()?;
        Self::open_instance(rm, sizes, client, 0)
    }

    /// The same on the GPU at this address, whose device file the RM must have open.
    pub fn open_at(mut rm: R, sizes: &'static HostBranch, address: &PciAddress) -> Result<Self, RmError> {
        let client = rm.new_client()?;
        let instance = device_instance_at(&mut rm, client, address)?;
        Self::open_instance(rm, sizes, client, instance)
    }

    /// By its address where that's known, and otherwise as [`Device::open`] does.
    pub fn open_place(rm: R, sizes: &'static HostBranch, place: &CardPlace) -> Result<Self, RmError> {
        match &place.address {
            Some(address) => Self::open_at(rm, sizes, address),
            None => Self::open(rm, sizes),
        }
    }

    fn open_instance(
        mut rm: R,
        sizes: &'static HostBranch,
        client: Handle,
        instance: u32,
    ) -> Result<Self, RmError> {
        let mut params = vec![0; sizes.device_params];
        put_u32(&mut params, DEVICE_PARAM_INSTANCE, instance);
        put_u32(&mut params, DEVICE_PARAM_VA_MODE, VA_MODE_MULTIPLE);
        let status = rm.alloc(client, DEVICE_HANDLE, DEVICE, &mut params)?;
        RmError::check(|| format!("class {DEVICE:#x}"), status)?;
        let mut params = vec![0; sizes.subdevice_params];
        let status = rm.alloc(DEVICE_HANDLE, SUBDEVICE_HANDLE, SUBDEVICE, &mut params)?;
        RmError::check(|| format!("class {SUBDEVICE:#x}"), status)?;
        Ok(Self {
            graphics_timeslice_micros: 0,
            rm,
            client,
            device: DEVICE_HANDLE,
            subdevice: SUBDEVICE_HANDLE,
            instance,
            doorbell_mapped: false,
            guest_clock: GuestClock::default(),
            next_handle: FIRST_OBJECT,
            sizes,
        })
    }

    pub fn new_handle(&mut self) -> Handle {
        let handle = self.next_handle;
        self.next_handle += 1;
        handle
    }

    /// Makes an object, and treats any status but success as failure.
    pub fn alloc(&mut self, parent: Handle, class: u32, params: &mut [u8]) -> Result<Handle, RmError> {
        let handle = self.new_handle();
        let status = self.rm.alloc(parent, handle, class, params)?;
        RmError::check(|| format!("class {class:#x}"), status)?;
        Ok(handle)
    }

    /// Runs a control, and treats any status but success as failure.
    pub fn control(&mut self, object: Handle, command: u32, params: &mut [u8]) -> Result<(), RmError> {
        let status = self.rm.control(object, command, params)?;
        RmError::check(|| format!("control {command:#x}"), status)
    }

    /// A control a guest may run, if it's on this branch's list.
    pub fn guest_control(&self, command: u32) -> Option<GuestControl> {
        self.sizes.guest_controls.iter().find(|control| control.command == command).copied()
    }

    /// The size of a control's parameters, if a guest may run it.
    pub fn guest_control_size(&self, command: u32) -> Option<usize> {
        self.guest_control(command).map(|control| control.params_size)
    }

    /// The size this branch's driver takes for a listed control.
    pub fn host_control_size(&self, command: u32) -> Option<usize> {
        self.guest_control(command).map(|control| control.host_size())
    }

    /// The object a control belongs on, from the class in the top half of its number.
    pub fn control_target(&self, command: u32) -> Handle {
        match command >> 16 {
            0 => self.client,
            0x80 => self.device,
            _ => self.subdevice,
        }
    }

    /// Makes video memory. Returns it, and the size the RM gave it.
    /// Makes video memory. A kind that isn't plain holds the attributes of a depth surface.
    pub fn alloc_video_memory(&mut self, size: u64, kind: MemoryKind) -> Result<(Handle, u64), RmError> {
        let mut params = vec![0; self.sizes.memory_params];
        put_u32(&mut params, MEMORY_PARAM_OWNER, MEMORY_OWNER);
        put_u32(&mut params, MEMORY_PARAM_FLAGS, MEMORY_FLAG_HANDLE_PROVIDED);
        put_u64(&mut params, MEMORY_PARAM_SIZE, size);
        if kind != MemoryKind::PLAIN {
            put_u32(&mut params, MEMORY_PARAM_TYPE, MEMORY_TYPE_DEPTH);
            put_u32(&mut params, MEMORY_PARAM_ATTRIBUTES, kind.0);
        }
        let handle = self.alloc(self.device, MEMORY_LOCAL_USER, &mut params)?;
        let given = params[MEMORY_PARAM_SIZE..MEMORY_PARAM_SIZE + 8].try_into().expect("eight bytes");
        Ok((handle, u64::from_le_bytes(given)))
    }

    pub fn alloc_host_pages(&mut self, address: u64, size: u64) -> Result<Handle, RmError> {
        let handle = self.new_handle();
        self.rm.alloc_host_pages(self.device, handle, address, size)?;
        Ok(handle)
    }

    /// Makes a GPU address space covering these ranges. A single range is limited, so there are two.
    pub fn create_address_space(&mut self, ranges: &[(u64, u64)]) -> Result<AddressSpace, RmError> {
        let handle = self.new_handle();
        let range = self.new_handle();
        let mut params = vec![0; self.sizes.address_space_params];
        put_u64(&mut params, ADDRESS_SPACE_PARAM_SIZE, ADDRESS_SPACE_SIZE);
        let status = self.rm.alloc(self.device, handle, VIRTUAL_ADDRESS_SPACE, &mut params)?;
        RmError::check(|| "GPU address space".to_owned(), status)?;
        let mut space = AddressSpace { handle, ranges: Vec::with_capacity(ranges.len()) };
        for (index, &(start, end)) in ranges.iter().enumerate() {
            let object = if index == 0 { range } else { self.new_handle() };
            self.alloc_range(object, handle, start, end - start)?;
            space.ranges.push((start, end, object));
        }
        Ok(space)
    }

    fn alloc_range(&mut self, range: Handle, space: Handle, base: u64, size: u64) -> Result<(), RmError> {
        let mut params = vec![0; self.sizes.memory_params];
        put_u32(&mut params, MEMORY_PARAM_OWNER, MEMORY_OWNER);
        put_u32(&mut params, MEMORY_PARAM_FLAGS, VIRTUAL_RANGE_FLAGS);
        put_u64(&mut params, MEMORY_PARAM_SIZE, size);
        put_u64(&mut params, MEMORY_PARAM_OFFSET, base);
        put_u32(&mut params, MEMORY_PARAM_ADDRESS_SPACE, space);
        let status = self.rm.alloc(self.device, range, MEMORY_VIRTUAL, &mut params)?;
        RmError::check(|| format!("virtual range at {base:#x}"), status)
    }

    pub fn destroy_address_space(&mut self, space: &AddressSpace) -> Result<(), RmError> {
        for &(_, _, range) in &space.ranges {
            self.rm.free(self.device, range)?;
        }
        self.rm.free(self.device, space.handle)
    }

    pub fn map(&mut self, space: &AddressSpace, map: &Mapping) -> Result<(), RmError> {
        let range = space.range_at(map.address)?;
        self.rm.map_dma(&DmaMap {
            device: self.device,
            range,
            memory: map.memory,
            offset: map.offset,
            length: map.length,
            address: map.address,
            small_pages: map.small_pages,
            gpu_cached: map.gpu_cached,
            page_kind: map.page_kind,
        })
    }

    pub fn unmap(
        &mut self,
        space: &AddressSpace,
        memory: Handle,
        address: u64,
        length: u64,
    ) -> Result<(), RmError> {
        let range = space.range_at(address)?;
        self.rm.unmap_dma(self.device, range, memory, address, length)
    }

    pub fn free(&mut self, handle: Handle) -> Result<(), RmError> {
        self.rm.free(self.device, handle)
    }

    /// Hands out video memory as a dmabuf, for a consumer on this card to read where it lies.
    pub fn export_dmabuf(&mut self, memory: Handle, size: u64) -> Result<OwnedFd, RmError> {
        if !size.is_multiple_of(EXPORT_GRAIN) {
            return Err(RmError::System(format!("{size:#x} bytes is not whole pages")));
        }
        self.rm.export_dmabuf(self.device, memory, size)
    }

    /// The GPU's clock in nanoseconds, which is what its timestamps count.
    pub fn gpu_time(&mut self) -> Result<u64, RmError> {
        let mut params = [0; 8];
        self.control(self.subdevice, CONTROL_GET_TIME, &mut params)?;
        Ok(u64::from_le_bytes(params))
    }

    /// The RM's number for this card.
    pub fn gpu_id(&mut self) -> Result<u32, RmError> {
        let mut params = [0; 4];
        self.control(self.subdevice, CONTROL_GET_GPU_ID, &mut params)?;
        Ok(u32::from_le_bytes(params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::HOST_615;
    use crate::rm::fake::{Call, FakeGpu, FakeRm};

    fn device() -> Device<FakeRm> {
        Device::open(FakeRm::new(), &HOST_615).unwrap()
    }

    #[test]
    fn opening_makes_the_device_and_subdevice_in_order() {
        let device = device();
        let classes: Vec<u32> = device.rm.allocated().map(|(class, _)| class).collect();
        assert_eq!(classes, [DEVICE, SUBDEVICE]);
    }

    #[test]
    fn the_device_asks_for_several_address_spaces() {
        let device = device();
        let (_, params) = device.rm.allocated().next().unwrap();
        assert_eq!(params[DEVICE_PARAM_VA_MODE], VA_MODE_MULTIPLE as u8);
        assert_eq!(params.len(), HOST_615.device_params);
    }

    fn instance_in(params: &[u8]) -> u32 {
        u32::from_le_bytes(params[DEVICE_PARAM_INSTANCE..DEVICE_PARAM_INSTANCE + 4].try_into().unwrap())
    }

    /// Two cards whose device files are in the other order from the RM's instances: the 4080's file is
    /// 0 and its instance 1, the 1660 SUPER's file 1 and its instance 0.
    fn two_cards() -> Vec<FakeGpu> {
        let at = |text: &str| PciAddress::parse(text).unwrap();
        vec![
            FakeGpu { id: 0x100, address: at("0000:01:00.0"), minor: 0, device_instance: 1, attached: true },
            FakeGpu { id: 0x200, address: at("0000:02:00.0"), minor: 1, device_instance: 0, attached: false },
        ]
    }

    #[test]
    fn a_one_card_pc_opens_instance_0_either_way() {
        let device = device();
        assert_eq!(instance_in(device.rm.allocated().next().unwrap().1), 0);
        let address = PciAddress::parse("0000:01:00.0").unwrap();
        let device = Device::open_at(FakeRm::new(), &HOST_615, &address).unwrap();
        assert_eq!(device.instance, 0);
        assert_eq!(instance_in(device.rm.allocated().next().unwrap().1), 0);
    }

    #[test]
    fn a_card_is_opened_by_the_instance_the_rm_gave_its_address() {
        for (minor, address, instance) in [(1, "0000:02:00.0", 0), (0, "0000:01:00.0", 1)] {
            let mut rm = FakeRm::with_gpus(two_cards());
            rm.open_device_file(minor);
            let place = CardPlace { minor, address: PciAddress::parse(address) };
            let device = Device::open_place(rm, &HOST_615, &place).unwrap();
            assert_eq!(device.instance, instance, "{address}");
            assert_eq!(instance_in(device.rm.allocated().next().unwrap().1), instance);
        }
    }

    #[test]
    fn the_device_file_number_as_the_instance_is_what_the_rm_refused() {
        let mut rm = FakeRm::with_gpus(two_cards());
        rm.open_device_file(1);
        let error = Device::open_instance(rm, &HOST_615, 1, 1).err().unwrap();
        assert!(error.to_string().contains("status 0x1b"), "{error}");
    }

    #[test]
    fn an_address_with_no_gpu_is_said_plainly() {
        let mut rm = FakeRm::with_gpus(two_cards());
        rm.open_device_file(1);
        let error =
            Device::open_at(rm, &HOST_615, &PciAddress::parse("0000:03:00.0").unwrap()).err().unwrap();
        assert!(error.to_string().contains("no GPU set up at 0000:03:00.0"), "{error}");
    }

    #[test]
    fn video_memory_reports_what_the_rm_gave() {
        let mut device = device();
        device.rm.round_memory_to = 0x10000;
        assert_eq!(device.alloc_video_memory(0x1234, MemoryKind::PLAIN).unwrap().1, 0x10000);
    }

    fn memory_params(device: &Device<FakeRm>) -> Vec<u8> {
        let (_, params) =
            device.rm.allocated().filter(|(class, _)| *class == MEMORY_LOCAL_USER).last().unwrap();
        params.to_vec()
    }

    fn u32_in(params: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(params[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn plain_memory_asks_for_no_type_or_attributes() {
        let mut device = device();
        device.alloc_video_memory(0x1000, MemoryKind::PLAIN).unwrap();
        let params = memory_params(&device);
        assert_eq!(u32_in(&params, MEMORY_PARAM_TYPE), 0);
        assert_eq!(u32_in(&params, MEMORY_PARAM_ATTRIBUTES), 0);
        assert_eq!(u32_in(&params, MEMORY_PARAM_FLAGS), MEMORY_FLAG_HANDLE_PROVIDED);
    }

    #[test]
    fn depth_memory_asks_for_the_depth_type_with_its_attributes() {
        let mut device = device();
        device.alloc_video_memory(0x40000, MemoryKind(0x8_0004)).unwrap();
        let params = memory_params(&device);
        assert_eq!(u32_in(&params, MEMORY_PARAM_TYPE), MEMORY_TYPE_DEPTH);
        assert_eq!(u32_in(&params, MEMORY_PARAM_ATTRIBUTES), 0x8_0004);
        assert_eq!(u32_in(&params, MEMORY_PARAM_ATTRIBUTES + 4), 0, "no second attribute word");
        assert_eq!(u32_in(&params, MEMORY_PARAM_OWNER), MEMORY_OWNER);
    }

    #[test]
    fn a_refusal_names_what_failed() {
        let mut device = device();
        device.rm.refuse_class = Some(MEMORY_LOCAL_USER);
        let error = device.alloc_video_memory(0x1000, MemoryKind(0x3A_0002)).unwrap_err();
        assert!(error.to_string().contains("0x40"), "{error}");
    }

    #[test]
    fn only_whole_pages_can_be_exported() {
        let mut device = device();
        assert!(device.export_dmabuf(7, 0x1800).is_err());
        assert!(device.rm.calls.iter().all(|call| !matches!(call, Call::ExportDmabuf { .. })));
        device.export_dmabuf(7, 0x2000).unwrap();
        assert!(device.rm.calls.contains(&Call::ExportDmabuf { memory: 7, size: 0x2000 }));
    }

    #[test]
    fn every_range_is_reserved_in_its_space() {
        let mut device = device();
        let space = device.create_address_space(&[(0x1000, 0x2000), (0x4000, 0x8000)]).unwrap();
        assert_eq!(space.ranges.len(), 2);
        assert_eq!(space.range_at(0x4000), Ok(space.ranges[1].2));
        assert!(space.range_at(0x3000).is_err());
    }

    #[test]
    fn a_mapping_goes_through_the_range_that_holds_it() {
        let mut device = device();
        let space = device.create_address_space(&[(0x1000, 0x2000), (0x4000, 0x8000)]).unwrap();
        let map =
            Mapping { memory: 7, length: 0x1000, address: 0x4000, small_pages: true, ..Mapping::default() };
        device.map(&space, &map).unwrap();
        let mapped = device.rm.calls.iter().find_map(|call| match call {
            Call::MapDma(map) => Some(*map),
            _ => None,
        });
        assert_eq!(mapped.map(|map| map.range), Some(space.ranges[1].2));
    }

    #[test]
    fn controls_go_to_the_object_their_number_names() {
        let device = device();
        assert_eq!(device.control_target(0x0000_0201), device.client);
        assert_eq!(device.control_target(0x0080_0292), device.device);
        assert_eq!(device.control_target(0x2080_0110), device.subdevice);
    }
}
