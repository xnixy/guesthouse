//! Class numbers and controls that are the same on every generation. All are in NVIDIA's public
//! open kernel module headers, under these names or close to them.

pub const ROOT_CLIENT: u32 = 0x41;
pub const DEVICE: u32 = 0x80;
pub const SUBDEVICE: u32 = 0x2080;
pub const MEMORY_LOCAL_USER: u32 = 0x40;
/// Plain system memory the RM itself allocates, which the kernel can map to the CPU: what a
/// semaphore surface's slots must live in.
pub const MEMORY_SYSTEM: u32 = 0x3E;
pub const VIRTUAL_ADDRESS_SPACE: u32 = 0x90F1;
pub const MEMORY_VIRTUAL: u32 = 0x50A0;
pub const CHANNEL_GROUP: u32 = 0xA06C;
pub const CONTEXT_SHARE: u32 = 0x9067;
pub const DEFERRED_API: u32 = 0x5080;

pub const CONTROL_GET_WORK_SUBMIT_TOKEN: u32 = 0xC36F_0108;
pub const CONTROL_SET_TIMESLICE: u32 = 0xA06C_0103;
pub const CONTROL_SCHEDULE_CHANNEL_GROUP: u32 = 0xA06C_0101;
pub const CONTROL_GET_CLASS_ENGINE: u32 = 0x906F_0101;
/// Keeps a control for the GPU to run when a channel reaches a method naming it.
pub const CONTROL_DEFERRED_API: u32 = 0x5080_0103;
/// Moves a channel to another ring. Meant to be deferred.
pub const CONTROL_UPDATE_CHANNEL_INFO: u32 = 0x2080_1116;
pub const CONTROL_GET_TIME: u32 = 0x2080_0403;
pub const CONTROL_GET_GPU_ID: u32 = 0x2080_0142;
pub const CONTROL_GET_PROBED_IDS: u32 = 0x0000_0214;
pub const CONTROL_GET_ATTACHED_IDS: u32 = 0x0000_0201;
/// The `_V2` form, whose parameters hold no pointer.
pub const CONTROL_GET_GPU_ID_INFO: u32 = 0x0000_0205;
pub const CONTROL_GET_GPU_PCI_INFO: u32 = 0x0000_021B;
pub const CONTROL_EXPORT_OBJECT_TO_FD: u32 = 0x0000_3D05;

/// The semaphore surface: the RM's monitored-fence object, whose waiters the kernel can post to a
/// file descriptor when a bound engine raises its non-stall interrupt.
pub const SEMAPHORE_SURFACE: u32 = 0xDA;
pub const SURFACE_BIND_CHANNEL: u32 = 0x00DA_0002;
pub const SURFACE_REGISTER_WAITER: u32 = 0x00DA_0003;
pub const SURFACE_UNREGISTER_WAITER: u32 = 0x00DA_0005;
pub const SURFACE_UNBIND_CHANNEL: u32 = 0x00DA_0006;
/// The RM's notifier indices a surface can be bound by. Copy is the one the daemon's own copies run
/// on; graphics and the FIFO round out the set a session would want.
pub const NOTIFY_GRAPHICS: u32 = 12;
pub const NOTIFY_COPY: u32 = 23;
pub const NOTIFY_FIFO_METHODS: u32 = 35;

/// The RM's engine numbers for the two engines channels run on here.
pub const ENGINE_GRAPHICS: u32 = 1;
pub const ENGINE_COPY_0: u32 = 9;

/// Entries of the bus information control that clear a counter when read. Their names end in `CLEAR`.
pub const BUS_ENTRIES_THAT_CLEAR: [u32; 8] = [0x20, 0x21, 0x22, 0x23, 0x28, 0x29, 0x2A, 0x2B];

/// No public names. A native encoding session takes a slot with the first when it opens and gives it
/// back with the second, which is how these are known: by watching one.
pub const CONTROL_ENCODE_SLOT_TAKE: u32 = 0x2080_8163;
pub const CONTROL_ENCODE_SLOT_GIVE_BACK: u32 = 0x2080_8164;
