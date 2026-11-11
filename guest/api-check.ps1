# Tries each graphics and compute API against the adapter and writes a plain text report of how far
# each got. Nothing is installed and nothing is downloaded: the calls are made through P/Invoke
# declarations compiled with Add-Type. Each API runs in its own child PowerShell process with a time
# limit, so a hang or a crash in one driver costs only that API's lines. The 64-bit PowerShell runs
# every API, and the 32-bit one runs Direct3D and OpenCL again, as the 32-bit drivers are a separate set.
#
# Usage: api-check.ps1 [-Report C:\gpupv\api-check.txt] [-TimeoutSeconds 60]
#        api-check.ps1 -SelfCheck     compiles every declaration and checks the struct sizes; calls no API
# The last line of a finished report is "api-check done".
param(
    [string]$Report = 'C:\gpupv\api-check.txt',
    [int]$TimeoutSeconds = 60,
    [switch]$SelfCheck,
    [string]$Probe,
    [string]$ProbeFile
)
$ErrorActionPreference = 'Continue'
$utf8 = New-Object System.Text.UTF8Encoding($false)

# ---- C# shared by every probe: one line at a time into the probe's file, so a crash keeps the lines before it.
$common = @'
public static class Rep {
    public static string Path;
    public static void Line(string text) { System.IO.File.AppendAllText(Path, text + "\r\n"); }
    public static string Hr(int value) { return "0x" + ((uint)value).ToString("X8"); }
    public static string Hr(uint value) { return "0x" + value.ToString("X8"); }
    public static string Ascii(byte[] data, int offset, int max) {
        int n = 0;
        while (n < max && offset + n < data.Length && data[offset + n] != 0) n++;
        return System.Text.Encoding.ASCII.GetString(data, offset, n);
    }
    public static void Threw(string call, Exception e) {
        Line("   !! " + call + " threw " + e.GetType().Name + ": " + e.Message);
    }
    public static string Layout(string name, int actual, int x64, int x86) {
        int expected = IntPtr.Size == 8 ? x64 : x86;
        return name + " " + actual + (actual == expected ? " ok" : " MISMATCH, header says " + expected);
    }
}
'@

# ---- Direct3D 12 and 11 on every DXGI adapter, and on the default adapter.
$d3d = @'
public static class Probe {
    public delegate int EnumAdapters1Fn(IntPtr self, uint index, out IntPtr adapter);
    public delegate int GetDesc1Fn(IntPtr self, out Desc1 desc);
    public delegate uint ReleaseFn(IntPtr self);

    // DXGI_ADAPTER_DESC1
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct Desc1 {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string Description;
        public uint VendorId; public uint DeviceId; public uint SubSysId; public uint Revision;
        public UIntPtr DedicatedVideoMemory; public UIntPtr DedicatedSystemMemory; public UIntPtr SharedSystemMemory;
        public uint LuidLow; public int LuidHigh; public uint Flags;
    }

    [DllImport("dxgi.dll")] static extern int CreateDXGIFactory1(ref Guid riid, out IntPtr factory);
    [DllImport("d3d12.dll")] static extern int D3D12CreateDevice(IntPtr adapter, uint minLevel, ref Guid riid, out IntPtr device);
    [DllImport("d3d11.dll")] static extern int D3D11CreateDevice(IntPtr adapter, uint driverType, IntPtr software, uint flags,
        uint[] levels, uint count, uint sdk, out IntPtr device, out uint level, out IntPtr context);

    static Delegate Fn(IntPtr obj, int slot, Type type) {
        IntPtr table = Marshal.ReadIntPtr(obj);
        return Marshal.GetDelegateForFunctionPointer(Marshal.ReadIntPtr(table, slot * IntPtr.Size), type);
    }
    static void Release(IntPtr obj) {
        if (obj != IntPtr.Zero) ((ReleaseFn)Fn(obj, 2, typeof(ReleaseFn)))(obj);
    }
    static string Level(uint level) {
        return (level >> 12) + "_" + ((level >> 8) & 0xF);
    }

    static void Try12(IntPtr adapter, string label, uint minLevel) {
        Guid iid = new Guid("189819f1-1db6-4b57-be54-1821339b85f7");
        IntPtr device;
        Rep.Line("-> D3D12CreateDevice(" + label + ", minimum feature level " + Level(minLevel) + ")");
        try {
            int hr = D3D12CreateDevice(adapter, minLevel, ref iid, out device);
            Rep.Line("<- D3D12CreateDevice = " + Rep.Hr(hr) + (hr == 0 ? " (device created)" : ""));
            if (hr == 0) Release(device);
        } catch (Exception e) { Rep.Threw("D3D12CreateDevice", e); }
    }

    static void Try11(IntPtr adapter, string label) {
        uint[] levels = new uint[] { 0xB100, 0xB000, 0xA100, 0xA000 };
        IntPtr device, context; uint got;
        // A given adapter needs the driver type to be "unknown" (0), the default adapter "hardware" (1).
        uint type = adapter == IntPtr.Zero ? 1u : 0u;
        Rep.Line("-> D3D11CreateDevice(" + label + ")");
        try {
            int hr = D3D11CreateDevice(adapter, type, IntPtr.Zero, 0, levels, (uint)levels.Length, 7, out device, out got, out context);
            Rep.Line("<- D3D11CreateDevice = " + Rep.Hr(hr) + (hr == 0 ? " (device created, feature level " + Level(got) + ")" : ""));
            if (hr == 0) { Release(context); Release(device); }
        } catch (Exception e) { Rep.Threw("D3D11CreateDevice", e); }
    }

    public static string Layout() {
        return Rep.Layout("DXGI_ADAPTER_DESC1", Marshal.SizeOf(typeof(Desc1)), 312, 296);
    }

    public static void Run(string path) {
        Rep.Path = path;
        Rep.Line("process: " + (IntPtr.Size * 8) + "-bit");
        Guid iid = new Guid("770aae78-f26f-4dba-a829-253c83d1b387");
        IntPtr factory = IntPtr.Zero;
        Rep.Line("-> CreateDXGIFactory1");
        try {
            int hr = CreateDXGIFactory1(ref iid, out factory);
            Rep.Line("<- CreateDXGIFactory1 = " + Rep.Hr(hr));
            if (hr == 0) {
                EnumAdapters1Fn enumerate = (EnumAdapters1Fn)Fn(factory, 12, typeof(EnumAdapters1Fn));
                for (uint i = 0; i < 16; i++) {
                    IntPtr adapter;
                    hr = enumerate(factory, i, out adapter);
                    if (hr == unchecked((int)0x887A0002)) { Rep.Line("EnumAdapters1(" + i + ") = DXGI_ERROR_NOT_FOUND, no more adapters"); break; }
                    if (hr < 0) { Rep.Line("EnumAdapters1(" + i + ") = " + Rep.Hr(hr)); break; }
                    Desc1 desc;
                    hr = ((GetDesc1Fn)Fn(adapter, 10, typeof(GetDesc1Fn)))(adapter, out desc);
                    string label = "adapter " + i;
                    if (hr == 0) {
                        Rep.Line(label + ": \"" + desc.Description + "\" vendor 0x" + desc.VendorId.ToString("X4") + " device 0x" + desc.DeviceId.ToString("X4")
                            + " subsys 0x" + desc.SubSysId.ToString("X8") + " revision " + desc.Revision + " dedicated " + (ulong)desc.DedicatedVideoMemory
                            + " shared " + (ulong)desc.SharedSystemMemory + " flags " + desc.Flags);
                    } else { Rep.Line(label + ": GetDesc1 = " + Rep.Hr(hr)); }
                    Try12(adapter, label, 0xB000);
                    Try12(adapter, label, 0xC000);
                    Try11(adapter, label);
                    Release(adapter);
                }
            }
        } catch (Exception e) { Rep.Threw("DXGI", e); }
        Release(factory);
        Try12(IntPtr.Zero, "default adapter", 0xB000);
        Try11(IntPtr.Zero, "default adapter");
        Rep.Line("finished");
    }
}
'@

# ---- OpenCL through OpenCL.dll. CL_API_CALL is __stdcall.
$opencl = @'
public static class Probe {
    const string Dll = "OpenCL.dll";
    [DllImport(Dll)] static extern int clGetPlatformIDs(uint n, IntPtr[] platforms, out uint count);
    [DllImport(Dll)] static extern int clGetPlatformInfo(IntPtr platform, uint name, UIntPtr size, byte[] value, out UIntPtr ret);
    [DllImport(Dll)] static extern int clGetDeviceIDs(IntPtr platform, ulong type, uint n, IntPtr[] devices, out uint count);
    [DllImport(Dll)] static extern int clGetDeviceInfo(IntPtr device, uint name, UIntPtr size, byte[] value, out UIntPtr ret);
    [DllImport(Dll)] static extern IntPtr clCreateContext(IntPtr props, uint n, IntPtr[] devices, IntPtr notify, IntPtr user, out int err);
    [DllImport(Dll)] static extern IntPtr clCreateCommandQueue(IntPtr context, IntPtr device, ulong props, out int err);
    [DllImport(Dll)] static extern IntPtr clCreateBuffer(IntPtr context, ulong flags, UIntPtr size, IntPtr host, out int err);
    [DllImport(Dll)] static extern IntPtr clCreateProgramWithSource(IntPtr context, uint count,
        [MarshalAs(UnmanagedType.LPArray, ArraySubType = UnmanagedType.LPStr)] string[] strings, IntPtr lengths, out int err);
    [DllImport(Dll)] static extern int clBuildProgram(IntPtr program, uint n, IntPtr[] devices,
        [MarshalAs(UnmanagedType.LPStr)] string options, IntPtr notify, IntPtr user);
    [DllImport(Dll)] static extern int clGetProgramBuildInfo(IntPtr program, IntPtr device, uint name, UIntPtr size, byte[] value, out UIntPtr ret);
    [DllImport(Dll)] static extern IntPtr clCreateKernel(IntPtr program, [MarshalAs(UnmanagedType.LPStr)] string name, out int err);
    [DllImport(Dll)] static extern int clSetKernelArg(IntPtr kernel, uint index, UIntPtr size, ref IntPtr value);
    [DllImport(Dll)] static extern int clEnqueueNDRangeKernel(IntPtr queue, IntPtr kernel, uint dim, IntPtr offset,
        UIntPtr[] global, IntPtr local, uint nwait, IntPtr waitList, IntPtr evt);
    [DllImport(Dll)] static extern int clEnqueueReadBuffer(IntPtr queue, IntPtr buffer, uint blocking, UIntPtr offset, UIntPtr size,
        byte[] dest, uint nwait, IntPtr waitList, IntPtr evt);
    [DllImport(Dll)] static extern int clFinish(IntPtr queue);
    [DllImport(Dll)] static extern int clReleaseKernel(IntPtr h);
    [DllImport(Dll)] static extern int clReleaseProgram(IntPtr h);
    [DllImport(Dll)] static extern int clReleaseMemObject(IntPtr h);
    [DllImport(Dll)] static extern int clReleaseCommandQueue(IntPtr h);
    [DllImport(Dll)] static extern int clReleaseContext(IntPtr h);

    const string Source = "__kernel void fill(__global int* out) { out[get_global_id(0)] = (int)get_global_id(0) + 1; }";

    static string PlatformText(IntPtr platform, uint what, string label) {
        byte[] b = new byte[512]; UIntPtr ret;
        int e = clGetPlatformInfo(platform, what, (UIntPtr)512, b, out ret);
        string text = e == 0 ? Rep.Ascii(b, 0, 512) : "";
        Rep.Line("   clGetPlatformInfo(" + label + ") = " + e + (e == 0 ? " \"" + text + "\"" : ""));
        return text;
    }
    static string DeviceText(IntPtr device, uint what, string label) {
        byte[] b = new byte[512]; UIntPtr ret;
        int e = clGetDeviceInfo(device, what, (UIntPtr)512, b, out ret);
        string text = e == 0 ? Rep.Ascii(b, 0, 512) : "";
        Rep.Line("   clGetDeviceInfo(" + label + ") = " + e + (e == 0 ? " \"" + text + "\"" : ""));
        return text;
    }
    static void DeviceNumber(IntPtr device, uint what, string label) {
        byte[] b = new byte[8]; UIntPtr ret;
        int e = clGetDeviceInfo(device, what, (UIntPtr)8, b, out ret);
        Rep.Line("   clGetDeviceInfo(" + label + ") = " + e + (e == 0 ? " " + BitConverter.ToUInt64(b, 0) + " (" + ret + " bytes)" : ""));
    }

    static void UseDevice(IntPtr device) {
        int err;
        IntPtr[] list = new IntPtr[] { device };
        Rep.Line("   -> clCreateContext");
        IntPtr context = clCreateContext(IntPtr.Zero, 1, list, IntPtr.Zero, IntPtr.Zero, out err);
        Rep.Line("   <- clCreateContext = " + err + (context != IntPtr.Zero ? " (context)" : " (null)"));
        if (context == IntPtr.Zero) return;
        Rep.Line("   -> clCreateCommandQueue");
        IntPtr queue = clCreateCommandQueue(context, device, 0, out err);
        Rep.Line("   <- clCreateCommandQueue = " + err + (queue != IntPtr.Zero ? " (queue)" : " (null)"));
        Rep.Line("   -> clCreateBuffer (256 bytes)");
        IntPtr buffer = clCreateBuffer(context, 1, (UIntPtr)256, IntPtr.Zero, out err);
        Rep.Line("   <- clCreateBuffer = " + err + (buffer != IntPtr.Zero ? " (buffer)" : " (null)"));
        Rep.Line("   -> clCreateProgramWithSource");
        IntPtr program = clCreateProgramWithSource(context, 1, new string[] { Source }, IntPtr.Zero, out err);
        Rep.Line("   <- clCreateProgramWithSource = " + err + (program != IntPtr.Zero ? " (program)" : " (null)"));
        if (program != IntPtr.Zero) {
            Rep.Line("   -> clBuildProgram");
            int built = clBuildProgram(program, 1, list, null, IntPtr.Zero, IntPtr.Zero);
            Rep.Line("   <- clBuildProgram = " + built);
            byte[] log = new byte[2048]; UIntPtr ret;
            int le = clGetProgramBuildInfo(program, device, 0x1183, (UIntPtr)2048, log, out ret);
            if (le == 0 && Rep.Ascii(log, 0, 2048).Length > 0) Rep.Line("      build log: " + Rep.Ascii(log, 0, 2048).Replace("\n", " "));
            if (built == 0) RunKernel(program, queue, buffer);
        }
        if (program != IntPtr.Zero) clReleaseProgram(program);
        if (buffer != IntPtr.Zero) clReleaseMemObject(buffer);
        if (queue != IntPtr.Zero) clReleaseCommandQueue(queue);
        clReleaseContext(context);
    }

    static void RunKernel(IntPtr program, IntPtr queue, IntPtr buffer) {
        int err;
        if (queue == IntPtr.Zero || buffer == IntPtr.Zero) { Rep.Line("   no kernel run: needs the queue and the buffer"); return; }
        Rep.Line("   -> clCreateKernel(fill)");
        IntPtr kernel = clCreateKernel(program, "fill", out err);
        Rep.Line("   <- clCreateKernel = " + err + (kernel != IntPtr.Zero ? " (kernel)" : " (null)"));
        if (kernel == IntPtr.Zero) return;
        IntPtr arg = buffer;
        int e = clSetKernelArg(kernel, 0, (UIntPtr)IntPtr.Size, ref arg);
        Rep.Line("   clSetKernelArg = " + e);
        Rep.Line("   -> clEnqueueNDRangeKernel (64 items)");
        e = clEnqueueNDRangeKernel(queue, kernel, 1, IntPtr.Zero, new UIntPtr[] { (UIntPtr)64 }, IntPtr.Zero, 0, IntPtr.Zero, IntPtr.Zero);
        Rep.Line("   <- clEnqueueNDRangeKernel = " + e);
        Rep.Line("   -> clFinish");
        e = clFinish(queue);
        Rep.Line("   <- clFinish = " + e);
        byte[] data = new byte[256];
        Rep.Line("   -> clEnqueueReadBuffer");
        e = clEnqueueReadBuffer(queue, buffer, 1, UIntPtr.Zero, (UIntPtr)256, data, 0, IntPtr.Zero, IntPtr.Zero);
        Rep.Line("   <- clEnqueueReadBuffer = " + e + (e == 0 ? ", first values " + BitConverter.ToInt32(data, 0) + " " + BitConverter.ToInt32(data, 4)
            + " " + BitConverter.ToInt32(data, 8) + " " + BitConverter.ToInt32(data, 12) + " (1 2 3 4 if the kernel ran)" : ""));
        clReleaseKernel(kernel);
    }

    public static string Layout() { return "OpenCL has no structs"; }

    public static void Run(string path) {
        Rep.Path = path;
        Rep.Line("process: " + (IntPtr.Size * 8) + "-bit");
        try {
            uint count;
            Rep.Line("-> clGetPlatformIDs");
            int e = clGetPlatformIDs(0, null, out count);
            Rep.Line("<- clGetPlatformIDs = " + e + ", " + count + " platform(s)");
            if (e != 0 || count == 0) return;
            IntPtr[] platforms = new IntPtr[count];
            e = clGetPlatformIDs(count, platforms, out count);
            Rep.Line("<- clGetPlatformIDs(list) = " + e);
            for (int p = 0; p < platforms.Length; p++) {
                Rep.Line("platform " + p);
                PlatformText(platforms[p], 0x0902, "NAME");
                PlatformText(platforms[p], 0x0903, "VENDOR");
                PlatformText(platforms[p], 0x0901, "VERSION");
                uint devices;
                Rep.Line("   -> clGetDeviceIDs(ALL)");
                e = clGetDeviceIDs(platforms[p], 0xFFFFFFFFUL, 0, null, out devices);
                Rep.Line("   <- clGetDeviceIDs = " + e + ", " + devices + " device(s)");
                if (e != 0 || devices == 0) continue;
                IntPtr[] list = new IntPtr[devices];
                e = clGetDeviceIDs(platforms[p], 0xFFFFFFFFUL, devices, list, out devices);
                Rep.Line("   <- clGetDeviceIDs(list) = " + e);
                for (int d = 0; d < list.Length; d++) {
                    Rep.Line("   device " + d);
                    DeviceText(list[d], 0x102B, "NAME");
                    DeviceText(list[d], 0x102C, "VENDOR");
                    DeviceText(list[d], 0x102F, "VERSION");
                    DeviceText(list[d], 0x102D, "DRIVER_VERSION");
                    DeviceNumber(list[d], 0x1000, "TYPE");
                    DeviceNumber(list[d], 0x1002, "MAX_COMPUTE_UNITS");
                    DeviceNumber(list[d], 0x101F, "GLOBAL_MEM_SIZE");
                    UseDevice(list[d]);
                }
            }
        } catch (Exception e) { Rep.Threw("OpenCL", e); }
        Rep.Line("finished");
    }
}
'@

# ---- Level Zero through ze_loader.dll. ZE_APICALL is __cdecl, and results are 32-bit unsigned.
$levelzero = @'
public static class Probe {
    const string Dll = "ze_loader.dll";
    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] static extern uint zeInit(uint flags);
    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] static extern uint zeDriverGet(ref uint count, IntPtr[] drivers);
    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] static extern uint zeDriverGetProperties(IntPtr driver, IntPtr props);
    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] static extern uint zeDeviceGet(IntPtr driver, ref uint count, IntPtr[] devices);
    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] static extern uint zeDeviceGetProperties(IntPtr device, IntPtr props);
    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] static extern uint zeContextCreate(IntPtr driver, IntPtr desc, out IntPtr context);
    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)] static extern uint zeContextDestroy(IntPtr context);

    // ze_driver_properties_t
    [StructLayout(LayoutKind.Sequential)]
    public struct DriverProps {
        public int stype; public IntPtr pNext;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 16)] public byte[] uuid;
        public uint driverVersion;
    }
    // ze_device_properties_t
    [StructLayout(LayoutKind.Sequential)]
    public struct DeviceProps {
        public int stype; public IntPtr pNext;
        public uint type; public uint vendorId; public uint deviceId; public uint flags; public uint subdeviceId; public uint coreClockRate;
        public ulong maxMemAllocSize; public uint maxHardwareContexts; public uint maxCommandQueuePriority;
        public uint numThreadsPerEU; public uint physicalEUSimdWidth; public uint numEUsPerSubslice; public uint numSubslicesPerSlice; public uint numSlices;
        public ulong timerResolution; public uint timestampValidBits; public uint kernelTimestampValidBits;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 16)] public byte[] uuid;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 256)] public byte[] name;
    }
    // ze_context_desc_t
    [StructLayout(LayoutKind.Sequential)]
    public struct ContextDesc { public int stype; public IntPtr pNext; public uint flags; }

    public static string Layout() {
        return Rep.Layout("ze_driver_properties_t", Marshal.SizeOf(typeof(DriverProps)), 40, 28) + "; "
            + Rep.Layout("ze_device_properties_t", Marshal.SizeOf(typeof(DeviceProps)), 368, 360) + "; "
            + Rep.Layout("ze_context_desc_t", Marshal.SizeOf(typeof(ContextDesc)), 24, 12);
    }

    // The driver writes into a block larger than the struct, so a wrong layout here shows as wrong
    // numbers in the report and not as a crash.
    static IntPtr Block() {
        IntPtr block = Marshal.AllocHGlobal(4096);
        for (int i = 0; i < 4096; i += 8) Marshal.WriteInt64(block, i, 0);
        return block;
    }

    public static void Run(string path) {
        Rep.Path = path;
        Rep.Line("process: " + (IntPtr.Size * 8) + "-bit");
        try {
            Rep.Line("-> zeInit(0)");
            uint r = zeInit(0);
            Rep.Line("<- zeInit = " + Rep.Hr(r));
            if (r != 0) return;
            uint drivers = 0;
            r = zeDriverGet(ref drivers, null);
            Rep.Line("<- zeDriverGet(count) = " + Rep.Hr(r) + ", " + drivers + " driver(s)");
            if (r != 0 || drivers == 0) return;
            IntPtr[] driverList = new IntPtr[drivers];
            r = zeDriverGet(ref drivers, driverList);
            Rep.Line("<- zeDriverGet(list) = " + Rep.Hr(r));
            for (int i = 0; i < driverList.Length; i++) {
                Rep.Line("driver " + i);
                IntPtr block = Block();
                DriverProps dp = new DriverProps(); dp.stype = 1; dp.uuid = new byte[16];
                Marshal.StructureToPtr(dp, block, false);
                r = zeDriverGetProperties(driverList[i], block);
                dp = (DriverProps)Marshal.PtrToStructure(block, typeof(DriverProps));
                Rep.Line("   zeDriverGetProperties = " + Rep.Hr(r) + (r == 0 ? ", driver version " + dp.driverVersion : ""));
                uint devices = 0;
                r = zeDeviceGet(driverList[i], ref devices, null);
                Rep.Line("   zeDeviceGet(count) = " + Rep.Hr(r) + ", " + devices + " device(s)");
                if (r == 0 && devices > 0) {
                    IntPtr[] deviceList = new IntPtr[devices];
                    r = zeDeviceGet(driverList[i], ref devices, deviceList);
                    Rep.Line("   zeDeviceGet(list) = " + Rep.Hr(r));
                    for (int d = 0; r == 0 && d < deviceList.Length; d++) {
                        DeviceProps props = new DeviceProps(); props.stype = 3; props.uuid = new byte[16]; props.name = new byte[256];
                        Marshal.StructureToPtr(props, block, false);
                        uint pr = zeDeviceGetProperties(deviceList[d], block);
                        props = (DeviceProps)Marshal.PtrToStructure(block, typeof(DeviceProps));
                        Rep.Line("   device " + d + ": zeDeviceGetProperties = " + Rep.Hr(pr)
                            + (pr == 0 ? ", \"" + Rep.Ascii(props.name, 0, 256) + "\" vendor 0x" + props.vendorId.ToString("X4") + " device 0x" + props.deviceId.ToString("X4")
                            + " type " + props.type + " slices " + props.numSlices + " subslices/slice " + props.numSubslicesPerSlice
                            + " EUs/subslice " + props.numEUsPerSubslice + " threads/EU " + props.numThreadsPerEU + " maxAlloc " + props.maxMemAllocSize : ""));
                    }
                }
                ContextDesc cd = new ContextDesc(); cd.stype = 0xd;
                Marshal.StructureToPtr(cd, block, false);
                IntPtr context;
                Rep.Line("   -> zeContextCreate");
                r = zeContextCreate(driverList[i], block, out context);
                Rep.Line("   <- zeContextCreate = " + Rep.Hr(r));
                if (r == 0) { r = zeContextDestroy(context); Rep.Line("   zeContextDestroy = " + Rep.Hr(r)); }
                Marshal.FreeHGlobal(block);
            }
        } catch (Exception e) { Rep.Threw("Level Zero", e); }
        Rep.Line("finished");
    }
}
'@

# ---- Vulkan through vulkan-1.dll. VKAPI_CALL is __stdcall.
$vulkan = @'
public static class Probe {
    const string Dll = "vulkan-1.dll";
    [StructLayout(LayoutKind.Sequential)]
    public struct AppInfo { public int sType; public IntPtr pNext; public IntPtr pApplicationName; public uint applicationVersion;
        public IntPtr pEngineName; public uint engineVersion; public uint apiVersion; }
    [StructLayout(LayoutKind.Sequential)]
    public struct InstanceInfo { public int sType; public IntPtr pNext; public uint flags; public IntPtr pApplicationInfo;
        public uint enabledLayerCount; public IntPtr ppEnabledLayerNames; public uint enabledExtensionCount; public IntPtr ppEnabledExtensionNames; }
    [StructLayout(LayoutKind.Sequential)]
    public struct QueueInfo { public int sType; public IntPtr pNext; public uint flags; public uint queueFamilyIndex; public uint queueCount; public IntPtr pQueuePriorities; }
    [StructLayout(LayoutKind.Sequential)]
    public struct DeviceInfo { public int sType; public IntPtr pNext; public uint flags; public uint queueCreateInfoCount; public IntPtr pQueueCreateInfos;
        public uint enabledLayerCount; public IntPtr ppEnabledLayerNames; public uint enabledExtensionCount; public IntPtr ppEnabledExtensionNames; public IntPtr pEnabledFeatures; }
    [StructLayout(LayoutKind.Sequential)]
    public struct QueueFamily { public uint queueFlags; public uint queueCount; public uint timestampValidBits; public uint granularityW; public uint granularityH; public uint granularityD; }

    [DllImport(Dll)] static extern int vkCreateInstance(ref InstanceInfo info, IntPtr allocator, out IntPtr instance);
    [DllImport(Dll)] static extern void vkDestroyInstance(IntPtr instance, IntPtr allocator);
    [DllImport(Dll)] static extern int vkEnumerateInstanceVersion(out uint version);
    [DllImport(Dll)] static extern int vkEnumeratePhysicalDevices(IntPtr instance, ref uint count, IntPtr[] devices);
    [DllImport(Dll)] static extern void vkGetPhysicalDeviceProperties(IntPtr device, IntPtr props);
    [DllImport(Dll)] static extern void vkGetPhysicalDeviceQueueFamilyProperties(IntPtr device, ref uint count, [Out] QueueFamily[] families);
    [DllImport(Dll)] static extern int vkCreateDevice(IntPtr physical, ref DeviceInfo info, IntPtr allocator, out IntPtr device);
    [DllImport(Dll)] static extern void vkGetDeviceQueue(IntPtr device, uint family, uint index, out IntPtr queue);
    [DllImport(Dll)] static extern void vkDestroyDevice(IntPtr device, IntPtr allocator);

    public static string Layout() {
        return Rep.Layout("VkApplicationInfo", Marshal.SizeOf(typeof(AppInfo)), 48, 28) + "; "
            + Rep.Layout("VkInstanceCreateInfo", Marshal.SizeOf(typeof(InstanceInfo)), 64, 32) + "; "
            + Rep.Layout("VkDeviceQueueCreateInfo", Marshal.SizeOf(typeof(QueueInfo)), 40, 24) + "; "
            + Rep.Layout("VkDeviceCreateInfo", Marshal.SizeOf(typeof(DeviceInfo)), 72, 40) + "; "
            + Rep.Layout("VkQueueFamilyProperties", Marshal.SizeOf(typeof(QueueFamily)), 24, 24);
    }

    static string[] Types = new string[] { "other", "integrated GPU", "discrete GPU", "virtual GPU", "CPU" };

    public static void Run(string path) {
        Rep.Path = path;
        Rep.Line("process: " + (IntPtr.Size * 8) + "-bit");
        IntPtr instance = IntPtr.Zero;
        try {
            try {
                uint version;
                int ve = vkEnumerateInstanceVersion(out version);
                Rep.Line("vkEnumerateInstanceVersion = " + ve + (ve == 0 ? ", " + (version >> 22) + "." + ((version >> 12) & 0x3FF) : ""));
            } catch (EntryPointNotFoundException) { Rep.Line("vkEnumerateInstanceVersion is not exported: a Vulkan 1.0 loader"); }

            AppInfo app = new AppInfo(); app.sType = 0; app.apiVersion = 1u << 22;
            app.pApplicationName = Marshal.StringToHGlobalAnsi("api-check");
            IntPtr appBlock = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(AppInfo)));
            Marshal.StructureToPtr(app, appBlock, false);
            InstanceInfo ii = new InstanceInfo(); ii.sType = 1; ii.pApplicationInfo = appBlock;
            Rep.Line("-> vkCreateInstance");
            int r = vkCreateInstance(ref ii, IntPtr.Zero, out instance);
            Rep.Line("<- vkCreateInstance = " + r);
            if (r != 0) return;
            uint count = 0;
            r = vkEnumeratePhysicalDevices(instance, ref count, null);
            Rep.Line("<- vkEnumeratePhysicalDevices(count) = " + r + ", " + count + " device(s)");
            if (r != 0 || count == 0) return;
            IntPtr[] devices = new IntPtr[count];
            r = vkEnumeratePhysicalDevices(instance, ref count, devices);
            Rep.Line("<- vkEnumeratePhysicalDevices(list) = " + r);
            IntPtr props = Marshal.AllocHGlobal(4096);
            for (int i = 0; i < devices.Length; i++) {
                for (int z = 0; z < 4096; z += 8) Marshal.WriteInt64(props, z, 0);
                vkGetPhysicalDeviceProperties(devices[i], props);
                byte[] name = new byte[256];
                Marshal.Copy(IntPtr.Add(props, 20), name, 0, 256);
                uint api = (uint)Marshal.ReadInt32(props, 0);
                int type = Marshal.ReadInt32(props, 16);
                Rep.Line("physical device " + i + ": \"" + Rep.Ascii(name, 0, 256) + "\" " + (type >= 0 && type < Types.Length ? Types[type] : "type " + type)
                    + " vendor 0x" + ((uint)Marshal.ReadInt32(props, 8)).ToString("X4") + " device 0x" + ((uint)Marshal.ReadInt32(props, 12)).ToString("X4")
                    + " API " + (api >> 22) + "." + ((api >> 12) & 0x3FF) + "." + (api & 0xFFF) + " driver 0x" + ((uint)Marshal.ReadInt32(props, 4)).ToString("X8"));
                uint families = 0;
                vkGetPhysicalDeviceQueueFamilyProperties(devices[i], ref families, null);
                QueueFamily[] list = new QueueFamily[families];
                vkGetPhysicalDeviceQueueFamilyProperties(devices[i], ref families, list);
                Rep.Line("   " + families + " queue family(ies)");
                int pick = -1;
                for (int f = 0; f < list.Length; f++) {
                    Rep.Line("   family " + f + ": flags 0x" + list[f].queueFlags.ToString("X") + " count " + list[f].queueCount);
                    if (pick < 0 && list[f].queueCount > 0) pick = f;
                }
                if (pick < 0) continue;
                IntPtr priority = Marshal.AllocHGlobal(4);
                Marshal.Copy(new float[] { 1.0f }, 0, priority, 1);
                QueueInfo qi = new QueueInfo(); qi.sType = 2; qi.queueFamilyIndex = (uint)pick; qi.queueCount = 1; qi.pQueuePriorities = priority;
                IntPtr qBlock = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(QueueInfo)));
                Marshal.StructureToPtr(qi, qBlock, false);
                DeviceInfo di = new DeviceInfo(); di.sType = 3; di.queueCreateInfoCount = 1; di.pQueueCreateInfos = qBlock;
                IntPtr device;
                Rep.Line("   -> vkCreateDevice (family " + pick + ", one queue)");
                r = vkCreateDevice(devices[i], ref di, IntPtr.Zero, out device);
                Rep.Line("   <- vkCreateDevice = " + r);
                if (r == 0) {
                    IntPtr queue;
                    vkGetDeviceQueue(device, (uint)pick, 0, out queue);
                    Rep.Line("   vkGetDeviceQueue -> " + (queue != IntPtr.Zero ? "a queue" : "null"));
                    vkDestroyDevice(device, IntPtr.Zero);
                }
            }
        } catch (Exception e) { Rep.Threw("Vulkan", e); }
        if (instance != IntPtr.Zero) { try { vkDestroyInstance(instance, IntPtr.Zero); } catch (Exception) { } }
        Rep.Line("finished");
    }
}
'@

# ---- OpenGL on a hidden window: a WGL context needs a device context and a pixel format, and no desktop.
$opengl = @'
public static class Probe {
    [StructLayout(LayoutKind.Sequential)]
    public struct PixelFormat {
        public ushort nSize; public ushort nVersion; public uint dwFlags; public byte iPixelType; public byte cColorBits;
        public byte cRedBits; public byte cRedShift; public byte cGreenBits; public byte cGreenShift; public byte cBlueBits; public byte cBlueShift;
        public byte cAlphaBits; public byte cAlphaShift; public byte cAccumBits; public byte cAccumRedBits; public byte cAccumGreenBits;
        public byte cAccumBlueBits; public byte cAccumAlphaBits; public byte cDepthBits; public byte cStencilBits; public byte cAuxBuffers;
        public byte iLayerType; public byte bReserved; public uint dwLayerMask; public uint dwVisibleMask; public uint dwDamageMask;
    }
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern IntPtr CreateWindowExW(uint exStyle, string cls, string title,
        uint style, int x, int y, int w, int h, IntPtr parent, IntPtr menu, IntPtr instance, IntPtr param);
    [DllImport("user32.dll")] static extern IntPtr GetDC(IntPtr window);
    [DllImport("user32.dll")] static extern int ReleaseDC(IntPtr window, IntPtr dc);
    [DllImport("user32.dll")] static extern bool DestroyWindow(IntPtr window);
    [DllImport("gdi32.dll")] static extern int ChoosePixelFormat(IntPtr dc, ref PixelFormat pfd);
    [DllImport("gdi32.dll")] static extern bool SetPixelFormat(IntPtr dc, int format, ref PixelFormat pfd);
    [DllImport("gdi32.dll")] static extern int DescribePixelFormat(IntPtr dc, int format, uint size, ref PixelFormat pfd);
    [DllImport("opengl32.dll")] static extern IntPtr wglCreateContext(IntPtr dc);
    [DllImport("opengl32.dll")] static extern bool wglMakeCurrent(IntPtr dc, IntPtr context);
    [DllImport("opengl32.dll")] static extern bool wglDeleteContext(IntPtr context);
    [DllImport("opengl32.dll")] static extern IntPtr glGetString(uint name);

    public static string Layout() { return Rep.Layout("PIXELFORMATDESCRIPTOR", Marshal.SizeOf(typeof(PixelFormat)), 40, 40); }

    static void Text(string label, uint name) {
        IntPtr p = glGetString(name);
        Rep.Line("   glGetString(" + label + ") = " + (p == IntPtr.Zero ? "null" : "\"" + Marshal.PtrToStringAnsi(p) + "\""));
    }

    public static void Run(string path) {
        Rep.Path = path;
        Rep.Line("process: " + (IntPtr.Size * 8) + "-bit");
        IntPtr window = IntPtr.Zero, dc = IntPtr.Zero, context = IntPtr.Zero;
        try {
            Rep.Line("-> CreateWindowExW (STATIC class, never shown)");
            window = CreateWindowExW(0, "STATIC", "api-check", 0x80000000u, 0, 0, 64, 64, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero);
            Rep.Line("<- CreateWindowExW = " + (window != IntPtr.Zero ? "a window" : "null, error " + Marshal.GetLastWin32Error()));
            if (window == IntPtr.Zero) return;
            dc = GetDC(window);
            Rep.Line("GetDC = " + (dc != IntPtr.Zero ? "a device context" : "null"));
            if (dc == IntPtr.Zero) return;
            PixelFormat pfd = new PixelFormat();
            pfd.nSize = (ushort)Marshal.SizeOf(typeof(PixelFormat)); pfd.nVersion = 1;
            pfd.dwFlags = 0x25; // PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER
            pfd.iPixelType = 0; pfd.cColorBits = 32; pfd.cDepthBits = 24;
            int format = ChoosePixelFormat(dc, ref pfd);
            Rep.Line("ChoosePixelFormat = " + format);
            if (format == 0) return;
            PixelFormat described = new PixelFormat();
            int n = DescribePixelFormat(dc, format, (uint)Marshal.SizeOf(typeof(PixelFormat)), ref described);
            Rep.Line("DescribePixelFormat = " + n + ", flags 0x" + described.dwFlags.ToString("X") + (n != 0 && (described.dwFlags & 0x40) != 0 ? " (generic: Microsoft's software OpenGL)" : ""));
            bool set = SetPixelFormat(dc, format, ref pfd);
            Rep.Line("SetPixelFormat = " + set);
            if (!set) return;
            Rep.Line("-> wglCreateContext");
            context = wglCreateContext(dc);
            Rep.Line("<- wglCreateContext = " + (context != IntPtr.Zero ? "a context" : "null, error " + Marshal.GetLastWin32Error()));
            if (context == IntPtr.Zero) return;
            bool current = wglMakeCurrent(dc, context);
            Rep.Line("wglMakeCurrent = " + current);
            if (current) {
                Text("VENDOR", 0x1F00); Text("RENDERER", 0x1F01); Text("VERSION", 0x1F02);
                wglMakeCurrent(IntPtr.Zero, IntPtr.Zero);
            }
        } catch (Exception e) { Rep.Threw("OpenGL", e); }
        try {
            if (context != IntPtr.Zero) wglDeleteContext(context);
            if (dc != IntPtr.Zero) ReleaseDC(window, dc);
            if (window != IntPtr.Zero) DestroyWindow(window);
        } catch (Exception) { }
        Rep.Line("finished");
    }
}
'@

$bodies = [ordered]@{ d3d = $d3d; opencl = $opencl; levelzero = $levelzero; vulkan = $vulkan; opengl = $opengl }

# Each probe is compiled alone, in a namespace of its own, so a mistake in one costs only that probe.
function Get-UnitSource([string]$key) {
    "using System; using System.Runtime.InteropServices; namespace Chk$key { $common $($bodies[$key]) }"
}

# ---- Child: one probe in this process, writing to its file.
if ($Probe) {
    try {
        Add-Type -TypeDefinition (Get-UnitSource $Probe) -Language CSharp
    } catch {
        [IO.File]::AppendAllText($ProbeFile, "compile failed: $($_.Exception.Message)`r`n", $utf8)
        exit 3
    }
    try {
        $type = [Type]"Chk$Probe.Probe"
        $null = $type.GetMethod('Run').Invoke($null, @($ProbeFile))
    } catch {
        $inner = if ($_.Exception.InnerException) { $_.Exception.InnerException } else { $_.Exception }
        [IO.File]::AppendAllText($ProbeFile, "probe failed: $($inner.GetType().Name): $($inner.Message)`r`n", $utf8)
        exit 4
    }
    exit 0
}

# ---- Dry run: compile everything, check the struct sizes against the headers, call no API.
if ($SelfCheck) {
    $bad = 0
    foreach ($key in $bodies.Keys) {
        try {
            Add-Type -TypeDefinition (Get-UnitSource $key) -Language CSharp
            $layout = ([Type]"Chk$key.Probe").GetMethod('Layout').Invoke($null, @())
            "$key compiled: $layout"
            if ($layout -match 'MISMATCH') { $bad++ }
        } catch {
            "$key FAILED to compile: $($_.Exception.Message)"
            $bad++
        }
    }
    "self-check: $(if ($bad) { "$bad problem(s)" } else { 'all fine' }), $([IntPtr]::Size * 8)-bit"
    exit $bad
}

# ---- Parent: runs every probe in a child process of its own, with a time limit, and writes the report.
$work = Join-Path (Split-Path -Parent $Report) 'api-check'
New-Item -ItemType Directory -Force $work | Out-Null
[IO.File]::WriteAllText($Report, '', $utf8)
function Add-Report([string]$text) { [IO.File]::AppendAllText($Report, "$text`r`n", $utf8) }

# A 32-bit parent reaches the real System32 as Sysnative.
$native = if ([Environment]::Is64BitProcess -or -not [Environment]::Is64BitOperatingSystem) { 'System32' } else { 'Sysnative' }
$shells = @{
    64 = Join-Path $env:windir "$native\WindowsPowerShell\v1.0\powershell.exe"
    32 = Join-Path $env:windir 'SysWOW64\WindowsPowerShell\v1.0\powershell.exe'
}

Add-Report "api-check started $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Add-Report "Windows $([Environment]::OSVersion.Version), user $([Environment]::UserName), session $((Get-Process -Id $PID).SessionId), PowerShell $($PSVersionTable.PSVersion), $([IntPtr]::Size * 8)-bit"
Add-Report 'loader files:'
foreach ($folder in 'System32', 'SysWOW64') {
    foreach ($name in 'OpenCL.dll', 'vulkan-1.dll', 'ze_loader.dll', 'opengl32.dll', 'dxgi.dll', 'd3d12.dll', 'd3d11.dll', 'msvcp140.dll', 'vcruntime140.dll') {
        $path = Join-Path (Join-Path $env:windir $folder) $name
        $info = if (Test-Path -LiteralPath $path) { "present, $((Get-Item -LiteralPath $path).VersionInfo.FileVersion)" } else { 'absent' }
        Add-Report "  $folder\$name $info"
    }
}

function Invoke-Probe([string]$probe, [int]$bits) {
    Add-Report ''
    Add-Report "-- $probe, $bits-bit PowerShell --"
    $exe = $shells[$bits]
    if (-not (Test-Path -LiteralPath $exe)) { Add-Report "   skipped: $exe does not exist"; return }
    $file = Join-Path $work "$probe-$bits.txt"
    $out = Join-Path $work "$probe-$bits.out"
    $err = Join-Path $work "$probe-$bits.err"
    Remove-Item -LiteralPath $file, $out, $err -Force -ErrorAction SilentlyContinue
    $arguments = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Probe', $probe, '-ProbeFile', "`"$file`"")
    try {
        $process = Start-Process -FilePath $exe -ArgumentList $arguments -PassThru -WindowStyle Hidden -RedirectStandardOutput $out -RedirectStandardError $err
        $null = $process.Handle
        if ($process.WaitForExit($TimeoutSeconds * 1000)) {
            $result = 'exit code {0} (0x{0:X8})' -f $process.ExitCode
        } else {
            try { $process.Kill() } catch { }
            $null = $process.WaitForExit(5000)
            $result = "TIMED OUT after $TimeoutSeconds seconds, process killed"
        }
    } catch {
        $result = "could not run: $($_.Exception.Message)"
    }
    if (Test-Path -LiteralPath $file) {
        foreach ($line in [IO.File]::ReadAllLines($file)) { Add-Report "  $line" }
    } else {
        Add-Report '  (the probe wrote nothing)'
    }
    foreach ($stream in $out, $err) {
        if ((Test-Path -LiteralPath $stream) -and (Get-Item -LiteralPath $stream).Length -gt 0) {
            $text = ([IO.File]::ReadAllText($stream)).Trim()
            if ($text.Length -gt 1500) { $text = $text.Substring(0, 1500) + ' ...' }
            Add-Report "  powershell said: $text"
        }
    }
    Add-Report "  result: $result"
}

Invoke-Probe 'd3d' 64
Invoke-Probe 'opencl' 64
Invoke-Probe 'levelzero' 64
Invoke-Probe 'vulkan' 64
Invoke-Probe 'opengl' 64
Invoke-Probe 'd3d' 32
Invoke-Probe 'opencl' 32
Add-Report ''
Add-Report 'api-check done'
