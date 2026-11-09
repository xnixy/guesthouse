//! The commands the daemon answers.

macro_rules! commands {
    ($($name:ident = $number:literal,)*) => {
        /// What a message asks for. Only commands the daemon handles have a name, and everything
        /// else is answered as not supported.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Command {
            $($name,)*
            Unknown(u32),
        }

        impl Command {
            pub fn from_wire(number: u32) -> Self {
                match number {
                    $($number => Self::$name,)*
                    other => Self::Unknown(other),
                }
            }

            pub fn to_wire(self) -> u32 {
                match self {
                    $(Self::$name => $number,)*
                    Self::Unknown(other) => other,
                }
            }
        }
    };
}

commands! {
    CreateDevice = 0,
    DestroyDevice = 1,
    QueryAdapter = 2,
    QueryDriverCaps = 3,
    CreateAllocation = 4,
    DestroyAllocation = 5,
    CreateContext = 6,
    DestroyContext = 7,
    CreateSyncObject = 8,
    CreatePagingQueue = 9,
    DestroyPagingQueue = 10,
    MakeResident = 11,
    Evict = 12,
    DriverEscape = 13,
    OpenAdapter = 14,
    CloseAdapter = 15,
    FreeGpuAddresses = 16,
    MapGpuAddresses = 17,
    ReserveGpuAddresses = 18,
    UpdateGpuAddresses = 19,
    SubmitToContext = 20,
    QueryVideoMemory = 21,
    WaitOnCpu = 22,
    LockForCpu = 23,
    UnlockForCpu = 24,
    WaitOnGpu = 25,
    SignalSyncObject = 26,
    SignalSharedFence = 27,
    MarkDeviceFailed = 29,
    StopAdapter = 30,
    SetQueuedLimit = 31,
    OpenResource = 32,
    SetContextPriority = 33,
    PresentToken = 34,
    SetFlipFenceValue = 35,
    GetAdapterIdentity = 36,
    FlushHeapTransitions = 37,
    CopyToStaging = 38,
    DescribeStandardAllocation = 39,
    FlushDevice = 42,
    FlushAdapter = 43,
    DescribeNode = 44,
    IsSyncObjectSignaled = 46,
    QueryStatistics = 48,
    ChangeVideoMemoryReservation = 49,
    CreateHardwareQueue = 50,
    DestroyHardwareQueue = 51,
    SubmitToHardwareQueue = 52,
    QueryClockCalibration = 62,
    GetRegistryKeys = 63,
    SetGuestPages = 66,
    CreateProcess = 1000,
    DestroyProcess = 1001,
    OpenSyncObject = 1002,
    DestroySyncObject = 1003,
    CreateSharedObject = 1004,
    DestroySharedObject = 1005,
    SignalFence = 1006,
    SetIoSpace = 1010,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_round_trip() {
        for number in [0, 20, 52, 66, 1000, 1010, 67, 4242] {
            assert_eq!(Command::from_wire(number).to_wire(), number);
        }
    }

    #[test]
    fn commands_without_a_handler_stay_unknown() {
        assert_eq!(Command::from_wire(68), Command::Unknown(68));
    }
}
