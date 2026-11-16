// SPDX-License-Identifier: GPL-3.0-or-later

//! Guest side of the ivshmem shared-memory transport (Windows only).
//!
//! * [`map_device`] / [`Bar`]: maps the BAR of the `ivshmem-plain` device through the Red Hat
//!   driver (`IOCTL_IVSHMEM_REQUEST_MMAP`, cached) after [`harden_devices`] restricted the device
//!   object to SYSTEM and Administrators (the driver's default DACL is open to Everyone).
//! * [`probe`]: read-only report of devices, sizes, DACLs and bound drivers.
//! * [`install_driver`] / [`uninstall_driver`]: opt-in, never automatic, driver package handling
//!   on top of `pnputil`.
//!
//! The measurements behind the driver usage are in `experiments/shmprobe/RESULTS.md`.

use std::ffi::{c_void, OsStr};
use std::fmt::{self, Write as _};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::io::Write as _;
use std::sync::{LazyLock, Mutex};

use tracing::warn;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW, SetupDiGetDevicePropertyW,
    SetupDiGetDeviceRegistryPropertyW, SetupGetInfInformationW, SetupQueryInfOriginalFileInformationW,
    DIGCF_ALLCLASSES, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, DN_HAS_PROBLEM, INFINFO_INF_NAME_IS_ABSOLUTE,
    SPDRP_HARDWAREID, SPDRP_SERVICE, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
    SP_INF_INFORMATION, SP_ORIGINAL_FILE_INFO_W,
};
use windows_sys::Win32::Devices::Properties::DEVPROPKEY;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Globalization::MultiByteToWideChar;
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::Cryptography::Catalog::{
    CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2, CryptCATAdminReleaseContext,
};
use windows_sys::Win32::Security::WinTrust::{
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_CATALOG_INFO, WINTRUST_DATA, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_CATALOG, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE,
    WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};
use windows_sys::Win32::Security::{SetKernelObjectSecurity, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetSystemWindowsDirectoryW};
use winbridge_shm::{Mapping, Region, Side, MAGIC};

/// Where `--ivshmem-output <file>` copies everything this module prints. The agent is a GUI
/// subsystem program that re-points its standard handles at the parent's console, so a caller
/// that redirects stdout (a service, a scheduled task, a test harness) would see nothing.
static OUTPUT: LazyLock<Mutex<Option<std::fs::File>>> = LazyLock::new(|| Mutex::new(None));

fn say(line: &str) {
    println!("{line}");
    if let Some(f) = OUTPUT.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = writeln!(f, "{line}");
    }
}

// Module-local `println!`: prints and copies to the `--ivshmem-output` file.
macro_rules! println {
    ($($arg:tt)*) => { say(&format!($($arg)*)) };
}

/// `GUID_DEVINTERFACE_IVSHMEM`.
const GUID_IVSHMEM: GUID = GUID::from_u128(0xdf576976_569d_4672_95a0_f57e4ea0b210);

/// CTL_CODE(FILE_DEVICE_UNKNOWN, func, METHOD_BUFFERED, FILE_ANY_ACCESS)
const fn ctl(func: u32) -> u32 {
    (0x22 << 16) | (func << 2)
}
const IOCTL_SIZE: u32 = ctl(0x801);
const IOCTL_MMAP: u32 = ctl(0x802);
const IOCTL_RELEASE: u32 = ctl(0x803);

/// `IVSHMEM_CACHE_CACHED`: coherent with the host and by far the fastest (RESULTS.md).
const CACHE_CACHED: u8 = 1;
const STATUS_DEVICE_ALREADY_ATTACHED: i32 = 0xC000_0038_u32 as i32;
const WRITE_DAC: u32 = 0x0004_0000;
const READ_CONTROL: u32 = 0x0002_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// DACL applied to the device objects: SYSTEM and Administrators only, no inheritance.
const HARDENED_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";
/// What the kernel reports back for [`HARDENED_SDDL`] (generic rights are mapped to FILE_ALL_ACCESS).
const HARDENED_READBACK: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)";

const PCI_HWID_PREFIX: &str = "PCI\\VEN_1AF4&DEV_1110";
const PACKAGE_INF: &str = "ivshmem.inf";
const PACKAGE_CAT: &str = "ivshmem.cat";
const PACKAGE_SYS: &str = "ivshmem.sys";
const PACKAGE_PROVIDER: &str = "Red Hat";
const PACKAGE_SERVICE: &str = "IVSHMEM";

/// Everything that can go wrong talking to the ivshmem driver or installing it.
#[derive(Debug)]
pub enum IvshmemError {
    /// A Win32 call failed.
    Win32 { context: String, code: u32 },
    /// A driver request failed with this NTSTATUS.
    Nt { context: String, status: i32 },
    /// No ivshmem device interface is present (no driver bound, or no device).
    NoDevice,
    /// Devices exist but none has the expected size.
    NoMatchingDevice { expected: u64, found: Vec<String> },
    /// Every matching device is mapped already (by this or another process).
    AlreadyAttached,
    /// The DACL could not be restricted or reads back wrong: shared memory must not be used.
    Hardening(String),
    /// The driver package failed signature verification.
    Signature(String),
    /// The package directory is unusable.
    Package(String),
    /// `pnputil` failed or its result did not verify.
    Pnputil(String),
    /// A device already has another driver bound.
    Conflict(String),
    /// Reading or reporting device state failed.
    Query(String),
}

impl fmt::Display for IvshmemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win32 { context, code } => write!(f, "{context}: Win32 error {code}"),
            Self::Nt { context, status } => write!(f, "{context}: {}", nt_name(*status)),
            Self::NoDevice => write!(f, "no ivshmem device interface present (driver not installed, or no ivshmem device attached)"),
            Self::NoMatchingDevice { expected, found } => {
                write!(f, "no ivshmem device with {expected} bytes (found: {})", found.join(", "))
            }
            Self::AlreadyAttached => write!(f, "every matching ivshmem device is already mapped (STATUS_DEVICE_ALREADY_ATTACHED)"),
            Self::Hardening(m) => write!(f, "cannot restrict the ivshmem device to SYSTEM and Administrators: {m}"),
            Self::Signature(m) => write!(f, "driver package signature check failed: {m}"),
            Self::Package(m) => write!(f, "driver package: {m}"),
            Self::Pnputil(m) => write!(f, "pnputil: {m}"),
            Self::Conflict(m) => write!(f, "{m}"),
            Self::Query(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for IvshmemError {}

fn win32(context: impl Into<String>, code: u32) -> IvshmemError {
    IvshmemError::Win32 { context: context.into(), code }
}

/// NTSTATUS as `0xC0000038 (STATUS_DEVICE_ALREADY_ATTACHED)`.
fn nt_name(s: i32) -> String {
    let name = match s as u32 {
        0 => "STATUS_SUCCESS",
        0xC000_0038 => "STATUS_DEVICE_ALREADY_ATTACHED",
        0xC000_0008 => "STATUS_INVALID_HANDLE",
        0xC000_000D => "STATUS_INVALID_PARAMETER",
        0xC000_0010 => "STATUS_INVALID_DEVICE_REQUEST",
        0xC000_0206 => "STATUS_INVALID_BUFFER_SIZE",
        0xC000_0022 => "STATUS_ACCESS_DENIED",
        0xC000_00E5 => "STATUS_INTERNAL_ERROR",
        0xC000_0182 => "STATUS_DRIVER_INTERNAL_ERROR",
        _ => "unknown",
    };
    format!("NTSTATUS 0x{:08X} ({name})", s as u32)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Decodes a NUL-terminated UTF-16 buffer.
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---------------------------------------------------------------------------------------------
// Handles and driver requests
// ---------------------------------------------------------------------------------------------

/// An owned kernel handle, closed on drop.
struct Handle(HANDLE);

// SAFETY: a kernel handle may be used and closed from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

#[repr(C)]
struct IoStatusBlock {
    status: usize,
    information: usize,
}

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtDeviceIoControlFile(
        file: HANDLE,
        event: HANDLE,
        apc: *mut c_void,
        apc_ctx: *mut c_void,
        iosb: *mut IoStatusBlock,
        code: u32,
        input: *mut c_void,
        in_len: u32,
        output: *mut c_void,
        out_len: u32,
    ) -> i32;
}

/// Synchronous ioctl on a non-overlapped handle; returns the NTSTATUS.
fn ioctl(h: &Handle, code: u32, input: &mut [u8], output: &mut [u8]) -> i32 {
    let mut iosb = IoStatusBlock { status: 0, information: 0 };
    let ptr_of = |b: &mut [u8]| if b.is_empty() { null_mut() } else { b.as_mut_ptr() as *mut c_void };
    // SAFETY: both buffers are valid for their lengths for the whole call; the handle is
    // synchronous, so the request has completed when the call returns.
    unsafe {
        NtDeviceIoControlFile(
            h.0,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut iosb,
            code,
            ptr_of(input),
            input.len() as u32,
            ptr_of(output),
            output.len() as u32,
        )
    }
}

fn open_path(path: &str, access: u32) -> Result<Handle, u32> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated.
    let h = unsafe { CreateFileW(w.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: no preconditions.
        return Err(unsafe { GetLastError() });
    }
    Ok(Handle(h))
}

fn device_size(h: &Handle) -> Result<u64, i32> {
    let mut o = [0u8; 8];
    match ioctl(h, IOCTL_SIZE, &mut [], &mut o) {
        st if st < 0 => Err(st),
        _ => Ok(u64::from_le_bytes(o)),
    }
}

// ---------------------------------------------------------------------------------------------
// Device interface enumeration
// ---------------------------------------------------------------------------------------------

struct DevInfoSet(isize);

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        // SAFETY: valid set returned by SetupDiGetClassDevsW, destroyed once.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn class_devs(class: *const GUID, enumerator: Option<&[u16]>, flags: u32) -> Result<DevInfoSet, IvshmemError> {
    let en = enumerator.map_or(null(), |e| e.as_ptr());
    // SAFETY: class/enumerator are null or valid for the call.
    let set = unsafe { SetupDiGetClassDevsW(class, en, null_mut(), flags) };
    if set == -1 {
        // SAFETY: no preconditions.
        return Err(win32("SetupDiGetClassDevs", unsafe { GetLastError() }));
    }
    Ok(DevInfoSet(set))
}

struct Iface {
    path: String,
    instance: String,
}

/// Every present device interface of the ivshmem driver.
fn interfaces() -> Result<Vec<Iface>, IvshmemError> {
    let set = class_devs(&GUID_IVSHMEM, None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)?;
    let mut out = Vec::new();
    let mut idx = 0;
    loop {
        // SAFETY: zeroed POD structure with its size field set.
        let mut did: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        did.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        // SAFETY: valid set and out structure.
        if unsafe { SetupDiEnumDeviceInterfaces(set.0, null(), &GUID_IVSHMEM, idx, &mut did) } == 0 {
            // SAFETY: no preconditions.
            let e = unsafe { GetLastError() };
            if e != 259 {
                // ERROR_NO_MORE_ITEMS ends the enumeration, anything else is a failure.
                return Err(win32("SetupDiEnumDeviceInterfaces", e));
            }
            break;
        }
        idx += 1;
        let mut need = 0u32;
        // SAFETY: size query with a null buffer.
        unsafe { SetupDiGetDeviceInterfaceDetailW(set.0, &did, null_mut(), 0, &mut need, null_mut()) };
        // 8-byte aligned buffer large enough for the variable-length structure.
        let mut buf = vec![0u64; (need as usize).div_ceil(8) + 1];
        let det = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        // SAFETY: `det` points into `buf`, which holds at least `need` bytes.
        unsafe { (*det).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        // SAFETY: zeroed POD structure with its size field set.
        let mut info: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        // SAFETY: `det` has `need` bytes available.
        if unsafe { SetupDiGetDeviceInterfaceDetailW(set.0, &did, det, need, null_mut(), &mut info) } == 0 {
            // SAFETY: no preconditions.
            return Err(win32("SetupDiGetDeviceInterfaceDetail", unsafe { GetLastError() }));
        }
        // SAFETY: the path is NUL-terminated inside `buf`; read through `need`-bounded slice.
        let path = unsafe {
            let p = std::ptr::addr_of!((*det).DevicePath) as *const u16;
            let max = (need as usize).saturating_sub(4) / 2;
            from_wide(std::slice::from_raw_parts(p, max))
        };
        out.push(Iface { path, instance: instance_id(&set, &info) });
    }
    Ok(out)
}

fn instance_id(set: &DevInfoSet, info: &SP_DEVINFO_DATA) -> String {
    let mut buf = [0u16; 512];
    let mut req = 0u32;
    // SAFETY: valid set/info, buffer length passed in characters.
    if unsafe { SetupDiGetDeviceInstanceIdW(set.0, info, buf.as_mut_ptr(), buf.len() as u32, &mut req) } != 0 {
        from_wide(&buf)
    } else {
        String::from("?")
    }
}

// ---------------------------------------------------------------------------------------------
// Security descriptor of the device object
// ---------------------------------------------------------------------------------------------

/// DACL of the device object as an SDDL string (`D:P(...)`).
fn read_dacl(h: &Handle) -> Result<String, u32> {
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: valid handle and out pointer; the descriptor is freed with LocalFree below.
    unsafe {
        let r = GetSecurityInfo(h.0, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION, null_mut(), null_mut(), null_mut(), null_mut(), &mut sd);
        if r != 0 {
            return Err(r);
        }
        let mut s: *mut u16 = null_mut();
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, DACL_SECURITY_INFORMATION, &mut s, null_mut());
        let err = GetLastError();
        let out = if ok != 0 && !s.is_null() {
            let mut n = 0;
            while *s.add(n) != 0 {
                n += 1;
            }
            Ok(String::from_utf16_lossy(std::slice::from_raw_parts(s, n)))
        } else {
            Err(err)
        };
        if !s.is_null() {
            LocalFree(s as _);
        }
        LocalFree(sd as _);
        out
    }
}

fn set_dacl(h: &Handle, sddl: &str) -> Result<(), u32> {
    let w = wide(sddl);
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: NUL-terminated SDDL and valid out pointer; the descriptor is freed with LocalFree.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) == 0 {
            return Err(GetLastError());
        }
        let ok = SetKernelObjectSecurity(h.0, DACL_SECURITY_INFORMATION, sd);
        let err = GetLastError();
        LocalFree(sd as _);
        if ok == 0 {
            return Err(err);
        }
    }
    Ok(())
}

/// Restricts the device object of every ivshmem interface whose region is `len` bytes (the control
/// region) to SYSTEM and Administrators and reads the DACL back. Returns the number of devices
/// hardened (0 when there is none). Devices of another size are not the agent's: the Venus slots
/// (`docs/IVSHMEM-TRANSPORT.md` section 8b) are mapped by the Vulkan processes of the interactive
/// user, who runs without the Administrators SID enabled, so they keep the driver's default DACL.
///
/// The driver's default DACL is open to Everyone and the change is not persistent (it applies to
/// the live device object), so the elevated agent calls this on every start, before mapping.
/// Any failure is an error: shared memory must then be treated as unavailable.
pub fn harden_devices(len: u64) -> Result<usize, IvshmemError> {
    let mut hardened = 0;
    for (i, f) in interfaces()?.iter().enumerate() {
        let what = format!("device {i} ({})", f.instance);
        let h = open_path(&f.path, GENERIC_READ | GENERIC_WRITE | WRITE_DAC)
            .map_err(|e| IvshmemError::Hardening(format!("{what}: open with WRITE_DAC failed, Win32 error {e} (is the agent elevated?)")))?;
        if device_size(&h) != Ok(len) {
            continue;
        }
        set_dacl(&h, HARDENED_SDDL).map_err(|e| IvshmemError::Hardening(format!("{what}: SetKernelObjectSecurity failed, Win32 error {e}")))?;
        let now = read_dacl(&h).map_err(|e| IvshmemError::Hardening(format!("{what}: reading the DACL back failed, Win32 error {e}")))?;
        if now != HARDENED_READBACK {
            return Err(IvshmemError::Hardening(format!("{what}: DACL reads back as {now}, expected {HARDENED_READBACK}")));
        }
        hardened += 1;
    }
    Ok(hardened)
}

// ---------------------------------------------------------------------------------------------
// The mapping
// ---------------------------------------------------------------------------------------------

/// A mapped ivshmem BAR. Dropping it releases the mapping (`RELEASE_MMAP`) and closes the
/// device handle, so another map in the same process succeeds afterwards.
pub struct Bar {
    base: *mut u8,
    len: usize,
    /// The handle that owns the driver's single mapping; released in `Drop` before it is closed.
    dev: Handle,
}

// SAFETY: the pointer refers to device memory that stays mapped until `drop`; the handle is a
// plain kernel handle. All access to the memory goes through `Mapping`'s raw pointer contract.
unsafe impl Send for Bar {}
unsafe impl Sync for Bar {}

// SAFETY: `base`/`len` come from a successful REQUEST_MMAP (non-null, size checked against
// IOCTL_SIZE), never change, and stay valid until `Drop` releases the mapping.
unsafe impl Mapping for Bar {
    fn base(&self) -> *mut u8 {
        self.base
    }
    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Bar {
    fn drop(&mut self) {
        let st = ioctl(&self.dev, IOCTL_RELEASE, &mut [], &mut []);
        if st < 0 {
            warn!("ivshmem RELEASE_MMAP failed: {}", nt_name(st));
        }
    }
}

impl Bar {
    /// Whether the region header magic is present (the host has initialised the region).
    fn has_magic(&self) -> bool {
        if self.len < 8 || (self.base as usize) % 8 != 0 {
            return false;
        }
        // SAFETY: in bounds and aligned (checked above); the host may write concurrently, hence volatile.
        let v = unsafe { std::ptr::read_volatile(self.base as *const u64) };
        u64::from_le(v) == MAGIC
    }
}

/// REQUEST_MMAP with the cached mode on `dev`.
fn request_mmap(dev: Handle, expected_len: u64) -> Result<Bar, IvshmemError> {
    let mut out = [0u8; 32];
    let st = ioctl(&dev, IOCTL_MMAP, &mut [CACHE_CACHED], &mut out);
    if st < 0 {
        return Err(IvshmemError::Nt { context: "REQUEST_MMAP(cached)".into(), status: st });
    }
    let size = u64::from_le_bytes(out[8..16].try_into().expect("8 bytes"));
    let ptr = u64::from_le_bytes(out[16..24].try_into().expect("8 bytes"));
    // From here on the driver holds the mapping for `dev`: build the Bar first so Drop releases it
    // on every error path below.
    let len = usize::try_from(size).unwrap_or(0);
    let bar = Bar { base: ptr as usize as *mut u8, len, dev };
    if bar.base.is_null() || size != expected_len || len == 0 {
        return Err(IvshmemError::Query(format!(
            "REQUEST_MMAP returned pointer {ptr:#x} and size {size}, expected {expected_len} bytes"
        )));
    }
    Ok(bar)
}

/// Maps the ivshmem device of `expected_len` bytes (cached).
///
/// Hardens the devices first (failure is an error). Among the devices whose size matches, the
/// one whose region header magic is already valid wins; otherwise the first device that is not
/// mapped yet. The mapping is held until the [`Bar`] is dropped.
pub fn map_device(expected_len: u64) -> Result<Bar, IvshmemError> {
    if harden_devices(expected_len)? == 0 {
        return Err(IvshmemError::NoDevice);
    }
    let mut sizes = Vec::new();
    let mut fallback: Option<Bar> = None;
    let mut attached = 0usize;
    let mut matching = 0usize;
    for f in interfaces()? {
        let h = open_path(&f.path, GENERIC_READ | GENERIC_WRITE).map_err(|e| win32(format!("open {}", f.instance), e))?;
        match device_size(&h) {
            Ok(s) if s == expected_len => matching += 1,
            Ok(s) => {
                sizes.push(format!("{} = {s}", f.instance));
                continue;
            }
            Err(st) => {
                sizes.push(format!("{}: IOCTL_SIZE {}", f.instance, nt_name(st)));
                continue;
            }
        }
        match request_mmap(h, expected_len) {
            Ok(bar) if bar.has_magic() => return Ok(bar),
            Ok(bar) => {
                // Keep the first uninitialised one; any later one is released by drop.
                fallback.get_or_insert(bar);
            }
            Err(IvshmemError::Nt { status, .. }) if status == STATUS_DEVICE_ALREADY_ATTACHED => attached += 1,
            Err(e) => return Err(e),
        }
    }
    if let Some(bar) = fallback {
        return Ok(bar);
    }
    if matching > 0 && attached == matching {
        return Err(IvshmemError::AlreadyAttached);
    }
    Err(IvshmemError::NoMatchingDevice { expected: expected_len, found: sizes })
}

// ---------------------------------------------------------------------------------------------
// Read-only status
// ---------------------------------------------------------------------------------------------

/// One ivshmem device interface as seen by [`probe`].
#[derive(Debug)]
pub struct DeviceReport {
    pub instance: String,
    pub size: Result<u64, String>,
    /// The DACL (`D:P(...)`) or why it could not be read.
    pub dacl: Result<String, String>,
    /// True when the DACL is exactly SYSTEM + Administrators.
    pub restricted: bool,
}

/// A PCI device with the ivshmem hardware id and the driver bound to it.
#[derive(Debug)]
pub struct PciDevice {
    pub instance: String,
    pub description: Option<String>,
    /// Published INF name of the bound driver (`oem4.inf`, or an inbox INF).
    pub inf: Option<String>,
    pub provider: Option<String>,
    pub service: Option<String>,
    /// Configuration manager problem code (0 = none); `None` when it could not be read.
    pub problem: Option<u32>,
}

/// Result of [`probe`].
#[derive(Debug)]
pub struct DeviceInfo {
    pub devices: Vec<DeviceReport>,
    pub pci: Vec<PciDevice>,
}

impl DeviceInfo {
    /// At least one device, and every device interface is restricted to SYSTEM and Administrators.
    pub fn hardened(&self) -> bool {
        !self.devices.is_empty() && self.devices.iter().all(|d| d.restricted)
    }

    /// Multi-line report for `--ivshmem-status`.
    pub fn report(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "ivshmem device interfaces: {}", self.devices.len());
        for (i, d) in self.devices.iter().enumerate() {
            let size = d.size.as_ref().map(|v| format!("{} MiB ({v} bytes)", v >> 20)).unwrap_or_else(|e| e.clone());
            let dacl = d.dacl.as_ref().map(|v| v.clone()).unwrap_or_else(|e| e.clone());
            let _ = writeln!(s, "  [{i}] {} size {size}; restricted to SYSTEM+Administrators: {}; DACL {dacl}", d.instance, d.restricted);
        }
        let _ = writeln!(s, "PCI devices {PCI_HWID_PREFIX}: {}", self.pci.len());
        for p in &self.pci {
            let _ = writeln!(s, "  {} \"{}\": {}", p.instance, p.description.as_deref().unwrap_or("?"), p.driver_text());
        }
        let _ = write!(s, "hardened: {}", self.hardened());
        s
    }
}

impl fmt::Display for DeviceInfo {
    /// One line for the agent log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ivshmem device(s)", self.devices.len())?;
        for d in &self.devices {
            match &d.size {
                Ok(v) => write!(f, " [{} MiB", v >> 20)?,
                Err(e) => write!(f, " [size: {e}")?,
            }
            write!(f, ", restricted={}]", d.restricted)?;
        }
        write!(f, ", hardened={}", self.hardened())
    }
}

impl PciDevice {
    fn driver_text(&self) -> String {
        let problem = match self.problem {
            Some(0) => "no problem".to_string(),
            Some(c) => format!("problem code {c}"),
            None => "problem state unknown".to_string(),
        };
        format!(
            "driver {} (provider {}, service {}), {problem}",
            self.inf.as_deref().unwrap_or("none"),
            self.provider.as_deref().unwrap_or("?"),
            self.service.as_deref().unwrap_or("none")
        )
    }
}

/// Reports the ivshmem devices, their sizes, DACLs and bound drivers. Read-only: nothing is
/// changed and no mapping is requested.
pub fn probe() -> Result<DeviceInfo, IvshmemError> {
    let mut devices = Vec::new();
    for f in interfaces()? {
        let opened = open_path(&f.path, GENERIC_READ | GENERIC_WRITE).or_else(|first| open_path(&f.path, READ_CONTROL).map_err(|_| first));
        let report = match opened {
            Ok(h) => {
                let dacl = read_dacl(&h).map_err(|e| format!("GetSecurityInfo: Win32 error {e}"));
                let restricted = dacl.as_deref() == Ok(HARDENED_READBACK);
                DeviceReport {
                    instance: f.instance,
                    size: device_size(&h).map_err(nt_name),
                    dacl,
                    restricted,
                }
            }
            Err(e) => DeviceReport {
                instance: f.instance,
                size: Err(format!("open failed: Win32 error {e}")),
                dacl: Err(format!("open failed: Win32 error {e}")),
                restricted: false,
            },
        };
        devices.push(report);
    }
    Ok(DeviceInfo { devices, pci: pci_devices()? })
}

// ---------------------------------------------------------------------------------------------
// PCI device and driver state
// ---------------------------------------------------------------------------------------------

const DEVPKEY_DRIVER_INF: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0xa8b865dd_2e3d_4094_ad97_e593a70c75d6), pid: 5 };
const DEVPKEY_DRIVER_PROVIDER: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0xa8b865dd_2e3d_4094_ad97_e593a70c75d6), pid: 9 };
const DEVPKEY_DEVICE_DESC: DEVPROPKEY = DEVPROPKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 2 };

fn string_property(set: &DevInfoSet, info: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
    let mut ty = 0u32;
    let mut req = 0u32;
    let mut buf = [0u16; 512];
    // SAFETY: valid set/info/key; the byte length of `buf` is passed.
    let ok = unsafe {
        SetupDiGetDevicePropertyW(set.0, info, key, &mut ty, buf.as_mut_ptr() as *mut u8, std::mem::size_of_val(&buf) as u32, &mut req, 0)
    };
    (ok != 0).then(|| from_wide(&buf)).filter(|s| !s.is_empty())
}

fn registry_property(set: &DevInfoSet, info: &SP_DEVINFO_DATA, prop: u32) -> Option<Vec<u16>> {
    let mut ty = 0u32;
    let mut req = 0u32;
    let mut buf = [0u16; 1024];
    // SAFETY: valid set/info; the byte length of `buf` is passed.
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(set.0, info, prop, &mut ty, buf.as_mut_ptr() as *mut u8, std::mem::size_of_val(&buf) as u32, &mut req)
    };
    if ok == 0 {
        return None;
    }
    let n = (req as usize / 2).min(buf.len());
    Some(buf[..n].to_vec())
}

/// Every present PCI device with hardware id `PCI\VEN_1AF4&DEV_1110`, with its driver binding.
fn pci_devices() -> Result<Vec<PciDevice>, IvshmemError> {
    let set = class_devs(null(), Some(&wide("PCI")), DIGCF_PRESENT | DIGCF_ALLCLASSES)?;
    let mut out = Vec::new();
    let mut idx = 0;
    loop {
        // SAFETY: zeroed POD structure with its size field set.
        let mut info: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        // SAFETY: valid set and out structure.
        if unsafe { SetupDiEnumDeviceInfo(set.0, idx, &mut info) } == 0 {
            // SAFETY: no preconditions.
            let e = unsafe { GetLastError() };
            if e != 259 {
                return Err(win32("SetupDiEnumDeviceInfo", e));
            }
            break;
        }
        idx += 1;
        let Some(ids) = registry_property(&set, &info, SPDRP_HARDWAREID) else { continue };
        let is_ivshmem = ids
            .split(|&c| c == 0)
            .map(String::from_utf16_lossy)
            .any(|id| id.len() >= PCI_HWID_PREFIX.len() && id[..PCI_HWID_PREFIX.len()].eq_ignore_ascii_case(PCI_HWID_PREFIX));
        if !is_ivshmem {
            continue;
        }
        let (mut status, mut problem) = (0u32, 0u32);
        // SAFETY: valid out pointers and device instance.
        let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, info.DevInst, 0) };
        out.push(PciDevice {
            instance: instance_id(&set, &info),
            description: string_property(&set, &info, &DEVPKEY_DEVICE_DESC),
            inf: string_property(&set, &info, &DEVPKEY_DRIVER_INF),
            provider: string_property(&set, &info, &DEVPKEY_DRIVER_PROVIDER),
            service: registry_property(&set, &info, SPDRP_SERVICE).map(|w| from_wide(&w)).filter(|s| !s.is_empty()),
            problem: (cr == 0).then_some(if status & DN_HAS_PROBLEM != 0 { problem } else { 0 }),
        });
    }
    Ok(out)
}

enum Binding {
    /// The Red Hat ivshmem driver.
    Ours,
    /// No driver, or an inbox one (`machine.inf`: "PCI standard RAM Controller").
    Unbound,
    /// A third-party driver.
    Other(String),
}

fn binding(d: &PciDevice) -> Binding {
    let Some(inf) = d.inf.as_deref() else { return Binding::Unbound };
    if !inf.to_ascii_lowercase().starts_with("oem") {
        return Binding::Unbound;
    }
    let ours = d.provider.as_deref().is_some_and(|p| p.contains(PACKAGE_PROVIDER))
        && d.service.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(PACKAGE_SERVICE));
    if ours {
        Binding::Ours
    } else {
        Binding::Other(d.driver_text())
    }
}

// ---------------------------------------------------------------------------------------------
// pnputil
// ---------------------------------------------------------------------------------------------

fn windows_dir() -> Result<PathBuf, IvshmemError> {
    let mut b = [0u16; 260];
    // SAFETY: buffer length passed in characters.
    let n = unsafe { GetSystemWindowsDirectoryW(b.as_mut_ptr(), b.len() as u32) } as usize;
    if n == 0 || n >= b.len() {
        // SAFETY: no preconditions.
        return Err(win32("GetSystemWindowsDirectory", unsafe { GetLastError() }));
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&b[..n])))
}

fn system_dir() -> Result<PathBuf, IvshmemError> {
    let mut b = [0u16; 260];
    // SAFETY: buffer length passed in characters.
    let n = unsafe { GetSystemDirectoryW(b.as_mut_ptr(), b.len() as u32) } as usize;
    if n == 0 || n >= b.len() {
        // SAFETY: no preconditions.
        return Err(win32("GetSystemDirectory", unsafe { GetLastError() }));
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&b[..n])))
}

/// Console output of the system tools is in the OEM code page.
fn oem_to_string(b: &[u8]) -> String {
    if b.is_empty() {
        return String::new();
    }
    const CP_OEMCP: u32 = 1;
    // SAFETY: lengths match the buffers.
    unsafe {
        let n = MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), b.len() as i32, null_mut(), 0);
        if n <= 0 {
            return String::from_utf8_lossy(b).into_owned();
        }
        let mut w = vec![0u16; n as usize];
        MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), b.len() as i32, w.as_mut_ptr(), n);
        String::from_utf16_lossy(&w)
    }
}

/// Runs `System32\pnputil.exe`, prints its output and returns the exit code.
fn run_pnputil(args: &[&OsStr]) -> Result<i32, IvshmemError> {
    let exe = system_dir()?.join("pnputil.exe");
    let shown: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    println!("> pnputil {}", shown.join(" "));
    let out = std::process::Command::new(&exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| IvshmemError::Pnputil(format!("cannot run {}: {e}", exe.display())))?;
    let mut text = oem_to_string(&out.stdout);
    text.push_str(&oem_to_string(&out.stderr));
    println!("{}", text.trim_end());
    let code = out.status.code().unwrap_or(-1);
    println!("pnputil exit code = {code}");
    Ok(code)
}

/// 0 success, 3010 success but a reboot is needed.
fn pnputil_ok(code: i32) -> bool {
    code == 0 || code == 3010
}

// ---------------------------------------------------------------------------------------------
// Signature verification
// ---------------------------------------------------------------------------------------------

fn trust_name(r: i32) -> String {
    let name = match r as u32 {
        0x800B_0100 => "TRUST_E_NOSIGNATURE",
        0x800B_0001 => "TRUST_E_PROVIDER_UNKNOWN",
        0x8009_6010 => "TRUST_E_BAD_DIGEST",
        0x800B_0109 => "CERT_E_UNTRUSTEDROOT",
        0x800B_0101 => "CERT_E_EXPIRED",
        0x800B_0004 => "TRUST_E_SUBJECT_NOT_TRUSTED",
        0x800B_0003 => "TRUST_E_SUBJECT_FORM_UNKNOWN",
        0x800B_010A => "CERT_E_CHAIN",
        0x8009_8000 => "TRUST_E_NOSIGNATURE (catalog)",
        _ => "unknown",
    };
    format!("WinVerifyTrust 0x{:08X} ({name})", r as u32)
}

/// Runs one WinVerifyTrust verify/close cycle on a prepared `WINTRUST_DATA`.
fn win_verify_trust(data: &mut WINTRUST_DATA) -> i32 {
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    data.cbStruct = std::mem::size_of::<WINTRUST_DATA>() as u32;
    data.dwUIChoice = WTD_UI_NONE;
    data.fdwRevocationChecks = WTD_REVOKE_NONE;
    // The guest may be offline: no revocation lookups, no network retrieval.
    data.dwProvFlags = WTD_REVOCATION_CHECK_NONE | WTD_CACHE_ONLY_URL_RETRIEVAL;
    data.dwStateAction = WTD_STATEACTION_VERIFY;
    // SAFETY: `data` and everything it points to are valid for both calls; -1 means no UI window.
    unsafe {
        let r = WinVerifyTrust(INVALID_HANDLE_VALUE, &mut action, data as *mut WINTRUST_DATA as *mut c_void);
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(INVALID_HANDLE_VALUE, &mut action, data as *mut WINTRUST_DATA as *mut c_void);
        r
    }
}

/// Embedded Authenticode signature of `path` (the `.cat`).
fn verify_embedded(path: &Path) -> Result<(), String> {
    let wpath = wide(&path.to_string_lossy());
    // SAFETY: zeroed POD structures filled in below.
    let mut file: WINTRUST_FILE_INFO = unsafe { std::mem::zeroed() };
    file.cbStruct = std::mem::size_of::<WINTRUST_FILE_INFO>() as u32;
    file.pcwszFilePath = wpath.as_ptr();
    // SAFETY: zeroed POD structure.
    let mut data: WINTRUST_DATA = unsafe { std::mem::zeroed() };
    data.dwUnionChoice = WTD_CHOICE_FILE;
    data.Anonymous.pFile = &mut file;
    match win_verify_trust(&mut data) {
        0 => Ok(()),
        r => Err(trust_name(r)),
    }
}

/// Verifies that `member` is listed in the signed catalog `cat` and that the catalog signature is
/// valid. Tries the SHA-256 and the SHA-1 member hash, as catalogs carry either.
fn verify_in_catalog(cat: &Path, member: &Path) -> Result<(), String> {
    let wcat = wide(&cat.to_string_lossy());
    let wmember = wide(&member.to_string_lossy());
    let f = open_path(&member.to_string_lossy(), GENERIC_READ).map_err(|e| format!("open {}: Win32 error {e}", member.display()))?;
    let mut last = String::from("no hash algorithm worked");
    for alg in ["SHA256", "SHA1"] {
        let walg = wide(alg);
        let mut admin: isize = 0;
        // SAFETY: valid out pointer and NUL-terminated algorithm name.
        if unsafe { CryptCATAdminAcquireContext2(&mut admin, null(), walg.as_ptr(), null(), 0) } == 0 {
            // SAFETY: no preconditions.
            last = format!("CryptCATAdminAcquireContext2({alg}): Win32 error {}", unsafe { GetLastError() });
            continue;
        }
        let result = (|| {
            let mut n = 0u32;
            // SAFETY: size query then fill; `hash` has `n` bytes.
            unsafe {
                CryptCATAdminCalcHashFromFileHandle2(admin, f.0, &mut n, null_mut(), 0);
                if n == 0 {
                    return Err(format!("hashing {} ({alg}) failed: Win32 error {}", member.display(), GetLastError()));
                }
                let mut hash = vec![0u8; n as usize];
                if CryptCATAdminCalcHashFromFileHandle2(admin, f.0, &mut n, hash.as_mut_ptr(), 0) == 0 {
                    return Err(format!("hashing {} ({alg}) failed: Win32 error {}", member.display(), GetLastError()));
                }
                let tag: String = hash.iter().map(|b| format!("{b:02X}")).collect();
                let wtag = wide(&tag);
                let mut ci: WINTRUST_CATALOG_INFO = std::mem::zeroed();
                ci.cbStruct = std::mem::size_of::<WINTRUST_CATALOG_INFO>() as u32;
                ci.pcwszCatalogFilePath = wcat.as_ptr();
                ci.pcwszMemberTag = wtag.as_ptr();
                ci.pcwszMemberFilePath = wmember.as_ptr();
                ci.hMemberFile = f.0;
                ci.pbCalculatedFileHash = hash.as_mut_ptr();
                ci.cbCalculatedFileHash = n;
                ci.hCatAdmin = admin;
                let mut data: WINTRUST_DATA = std::mem::zeroed();
                data.dwUnionChoice = WTD_CHOICE_CATALOG;
                data.Anonymous.pCatalog = &mut ci;
                match win_verify_trust(&mut data) {
                    0 => Ok(()),
                    r => Err(format!("{alg} hash of {}: {}", member.display(), trust_name(r))),
                }
            }
        })();
        // SAFETY: context acquired above.
        unsafe { CryptCATAdminReleaseContext(admin, 0) };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The signature check that must pass before anything is installed: the catalog carries a valid
/// signature and lists both the INF and the driver binary with matching hashes.
fn verify_package(dir: &Path) -> Result<(), IvshmemError> {
    let cat = dir.join(PACKAGE_CAT);
    verify_embedded(&cat).map_err(|e| IvshmemError::Signature(format!("{}: {e}", cat.display())))?;
    println!("signature OK: {PACKAGE_CAT} (embedded signature valid)");
    for name in [PACKAGE_INF, PACKAGE_SYS] {
        let p = dir.join(name);
        verify_in_catalog(&cat, &p).map_err(|e| IvshmemError::Signature(format!("{}: {e}", p.display())))?;
        println!("signature OK: {name} (hash listed in {PACKAGE_CAT})");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Driver store
// ---------------------------------------------------------------------------------------------

fn inf_text(path: &Path) -> Option<String> {
    let b = std::fs::read(path).ok()?;
    if b.starts_with(&[0xFF, 0xFE]) {
        let w: Vec<u16> = b[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return Some(String::from_utf16_lossy(&w));
    }
    Some(String::from_utf8_lossy(&b).into_owned())
}

/// Original file name of a published INF (`oemNN.inf` -> `ivshmem.inf`).
fn original_inf_name(path: &Path) -> Option<String> {
    let w = wide(&path.to_string_lossy());
    let mut need = 0u32;
    // SAFETY: size query with a null buffer.
    unsafe { SetupGetInfInformationW(w.as_ptr() as *const c_void, INFINFO_INF_NAME_IS_ABSOLUTE, null_mut(), 0, &mut need) };
    if need == 0 {
        return None;
    }
    let mut buf = vec![0u64; (need as usize).div_ceil(8)];
    let info = buf.as_mut_ptr() as *mut SP_INF_INFORMATION;
    // SAFETY: `buf` holds `need` bytes, 8-byte aligned.
    if unsafe { SetupGetInfInformationW(w.as_ptr() as *const c_void, INFINFO_INF_NAME_IS_ABSOLUTE, info, need, &mut need) } == 0 {
        return None;
    }
    // SAFETY: zeroed POD structure with its size field set.
    let mut orig: SP_ORIGINAL_FILE_INFO_W = unsafe { std::mem::zeroed() };
    orig.cbSize = std::mem::size_of::<SP_ORIGINAL_FILE_INFO_W>() as u32;
    // SAFETY: `info` was filled in by SetupGetInfInformationW.
    if unsafe { SetupQueryInfOriginalFileInformationW(info, 0, null(), &mut orig) } == 0 {
        return None;
    }
    let name = orig.OriginalInfName;
    Some(from_wide(&name))
}

/// Published names (`oem4.inf`) of our packages: original name `ivshmem.inf`, provider Red Hat.
fn our_packages() -> Result<Vec<String>, IvshmemError> {
    let dir = windows_dir()?.join("INF");
    let rd = std::fs::read_dir(&dir).map_err(|e| IvshmemError::Query(format!("cannot list {}: {e}", dir.display())))?;
    let mut found = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let l = name.to_ascii_lowercase();
        if !(l.starts_with("oem") && l.ends_with(".inf")) {
            continue;
        }
        let path = e.path();
        if original_inf_name(&path).is_some_and(|o| o.eq_ignore_ascii_case(PACKAGE_INF))
            && inf_text(&path).is_some_and(|t| t.contains(PACKAGE_PROVIDER))
        {
            found.push(name);
        }
    }
    found.sort();
    Ok(found)
}

fn print_devices(devs: &[PciDevice]) {
    if devs.is_empty() {
        println!("no PCI device {PCI_HWID_PREFIX} present");
    }
    for d in devs {
        println!("device {} \"{}\": {}", d.instance, d.description.as_deref().unwrap_or("?"), d.driver_text());
    }
}

/// Installs the signed package from `package_dir` (`ivshmem.inf`, `.cat`, `.sys`).
///
/// Steps: signature check (before anything touches the system), refusal when a device has
/// another driver bound (unless `force`), `pnputil /add-driver <inf> /install`, then every
/// `PCI\VEN_1AF4&DEV_1110` device must be bound to the package and report no problem code.
/// Installing again when everything is in place prints "already installed" and changes nothing.
pub fn install_driver(package_dir: &Path, force: bool) -> Result<(), IvshmemError> {
    for name in [PACKAGE_INF, PACKAGE_CAT, PACKAGE_SYS] {
        let p = package_dir.join(name);
        if !p.is_file() {
            return Err(IvshmemError::Package(format!("{} is missing", p.display())));
        }
    }
    verify_package(package_dir)?;

    let before = pci_devices()?;
    print_devices(&before);
    for d in &before {
        if let Binding::Other(what) = binding(d) {
            if force {
                println!("--force: continuing although {} has another driver bound ({what})", d.instance);
            } else {
                return Err(IvshmemError::Conflict(format!(
                    "{} already has a different driver bound ({what}); not touching it (use --force to install anyway)",
                    d.instance
                )));
            }
        }
    }
    if !before.is_empty() && before.iter().all(|d| matches!(binding(d), Binding::Ours) && d.problem == Some(0)) {
        println!("already installed: every ivshmem device is bound to the Red Hat driver without problems; nothing to do");
        return Ok(());
    }

    let inf = package_dir.join(PACKAGE_INF);
    let code = run_pnputil(&[OsStr::new("/add-driver"), inf.as_os_str(), OsStr::new("/install")])?;
    // 259 (ERROR_NO_MORE_ITEMS): staged, but there was no device to install it on; the check below reports that.
    if !pnputil_ok(code) && code != 259 {
        return Err(IvshmemError::Pnputil(format!("/add-driver failed with exit code {code} (output above)")));
    }
    if code == 3010 {
        println!("a reboot is required to finish the installation");
    }

    let after = pci_devices()?;
    print_devices(&after);
    if after.is_empty() {
        return Err(IvshmemError::Pnputil(format!(
            "package staged, but no PCI device {PCI_HWID_PREFIX} is present to verify it on (is the ivshmem device attached to the VM?)"
        )));
    }
    for d in &after {
        match (binding(d), d.problem) {
            (Binding::Ours, Some(0)) => {}
            (Binding::Ours, p) => {
                return Err(IvshmemError::Pnputil(format!(
                    "{} is bound to the driver but reports problem code {} (CM_PROB_UNSIGNED_DRIVER is 52, FAILED_INSTALL 28)",
                    d.instance,
                    p.map_or("unknown".to_string(), |c| c.to_string())
                )))
            }
            _ => {
                return Err(IvshmemError::Pnputil(format!("{} is not bound to the installed driver: {}", d.instance, d.driver_text())))
            }
        }
    }
    println!("installed: {} device(s) bound to the Red Hat ivshmem driver, no problem code", after.len());
    Ok(())
}

/// Removes our package from the driver store and from the devices using it
/// (`pnputil /delete-driver oemNN.inf /uninstall`). Packages are found by original INF name
/// `ivshmem.inf` with provider Red Hat; nothing else is touched. Says clearly when there was
/// nothing to remove.
pub fn uninstall_driver() -> Result<(), IvshmemError> {
    let pkgs = our_packages()?;
    if pkgs.is_empty() {
        println!("nothing to uninstall: no {PACKAGE_INF} package from {PACKAGE_PROVIDER} is installed in the driver store");
        return Ok(());
    }
    let mut reboot = false;
    for p in &pkgs {
        println!("found package {p} (original name {PACKAGE_INF}, provider {PACKAGE_PROVIDER})");
        let code = run_pnputil(&[OsStr::new("/delete-driver"), OsStr::new(p), OsStr::new("/uninstall")])?;
        if !pnputil_ok(code) {
            return Err(IvshmemError::Pnputil(format!("/delete-driver {p} failed with exit code {code} (output above)")));
        }
        reboot |= code == 3010;
    }
    if reboot {
        println!("a reboot is required to finish the removal");
    }
    let left = our_packages()?;
    if !left.is_empty() {
        return Err(IvshmemError::Pnputil(format!("package(s) still in the driver store: {}", left.join(", "))));
    }
    println!("uninstalled: {} package(s) removed", pkgs.len());
    print_devices(&pci_devices()?);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------------------------

/// Handles `--install-ivshmem <dir> [--force]`, `--uninstall-ivshmem`, `--ivshmem-status` and
/// `--ivshmem-selftest [user]`. Returns the exit code when one of them was given.
pub fn run_cli(args: &[String]) -> Option<i32> {
    let has = |f: &str| args.iter().any(|a| a == f);
    const COMMANDS: [&str; 4] = ["--install-ivshmem", "--uninstall-ivshmem", "--ivshmem-status", "--ivshmem-selftest"];
    if !args.iter().any(|a| COMMANDS.contains(&a.as_str())) {
        return None;
    }
    if let Some(path) = args.iter().position(|a| a == "--ivshmem-output").and_then(|i| args.get(i + 1)) {
        match std::fs::File::create(path) {
            Ok(f) => *OUTPUT.lock().unwrap_or_else(|e| e.into_inner()) = Some(f),
            Err(e) => println!("ERROR: cannot create {path}: {e}"),
        }
    }
    let report = |r: Result<(), IvshmemError>| match r {
        Ok(()) => 0,
        Err(e) => {
            println!("ERROR: {e}");
            1
        }
    };
    if let Some(i) = args.iter().position(|a| a == "--install-ivshmem") {
        let Some(dir) = args.get(i + 1).filter(|d| !d.starts_with("--")) else {
            println!("ERROR: --install-ivshmem needs the directory with ivshmem.inf, ivshmem.cat and ivshmem.sys");
            return Some(2);
        };
        return Some(report(install_driver(Path::new(dir), has("--force"))));
    }
    if has("--uninstall-ivshmem") {
        return Some(report(uninstall_driver()));
    }
    if has("--ivshmem-status") {
        return Some(report(probe().map(|i| println!("{}", i.report()))));
    }
    if let Some(i) = args.iter().position(|a| a == "--ivshmem-selftest") {
        return Some(selftest(args.get(i + 1).is_some_and(|m| m == "user")));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Self-test (run in the build VM; see docs/ARCHITECTURE.md)
// ---------------------------------------------------------------------------------------------

const SELFTEST_LEN: u64 = 256 << 20;

fn pattern_byte(i: usize) -> u8 {
    (i.wrapping_mul(31).wrapping_add(7) & 0xFF) as u8
}

/// Writes then reads back a 4 KiB pattern at `offset` through the mapping.
fn pattern_roundtrip(bar: &Bar, offset: usize) -> Result<(), String> {
    if offset + 4096 > bar.len() {
        return Err("offset outside the mapping".into());
    }
    // SAFETY: in bounds (checked); the memory is ours to write at this offset in the test.
    unsafe {
        let p = bar.base().add(offset);
        for i in 0..4096 {
            std::ptr::write_volatile(p.add(i), pattern_byte(i));
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        for i in 0..4096 {
            let v = std::ptr::read_volatile(p.add(i));
            if v != pattern_byte(i) {
                return Err(format!("byte {i} at offset {offset:#x} reads {v:#x}, wrote {:#x}", pattern_byte(i)));
            }
        }
    }
    Ok(())
}

/// `--ivshmem-selftest user`: as an unprivileged user a device of the control size must refuse to
/// open; other devices (the Venus slots) keep the driver's default DACL and may open, which the
/// report shows. Plain `--ivshmem-selftest` (SYSTEM / elevated): harden, map the devices of the
/// control size, pattern, header, single-mapping rule and release-on-drop. Exit code 0 only if
/// every step passed.
fn selftest(as_user: bool) -> i32 {
    let mut failed = 0;
    let mut step = |name: &str, r: Result<String, String>| match r {
        Ok(m) => println!("PASS {name}: {m}"),
        Err(m) => {
            failed += 1;
            println!("FAIL {name}: {m}");
        }
    };
    if as_user {
        let ifaces = interfaces();
        step("enumerate", ifaces.as_ref().map(|v| format!("{} interface(s)", v.len())).map_err(|e| e.to_string()));
        for f in ifaces.unwrap_or_default() {
            let r = match open_path(&f.path, GENERIC_READ | GENERIC_WRITE) {
                Ok(h) => match device_size(&h) {
                    Ok(SELFTEST_LEN) => Err("a device of the control size was OPENED by an unprivileged user".to_string()),
                    Ok(len) => Ok(format!("opened, {len} bytes (not a control device)")),
                    Err(st) => Err(format!("opened, but its size cannot be read: {}", nt_name(st))),
                },
                Err(5) => Ok("ACCESS_DENIED (Win32 error 5)".to_string()),
                Err(e) => Err(format!("unexpected Win32 error {e}")),
            };
            step(&format!("device {}", f.instance), r);
        }
        println!("{}", if failed == 0 { "SELFTEST USER OK" } else { "SELFTEST USER FAILED" });
        return failed.min(1);
    }

    let devices = match harden_devices(SELFTEST_LEN) {
        Ok(n) => {
            step("harden", Ok(format!("{n} device(s) restricted")));
            n
        }
        Err(e) => {
            step("harden", Err(e.to_string()));
            0
        }
    };
    // Every device maps once; the next mapping is refused while all are held.
    let mut held = Vec::new();
    for i in 0..devices {
        match map_device(SELFTEST_LEN) {
            Ok(bar) => {
                step(&format!("map device {}", i + 1), Ok(format!("{:p} len {}", bar.base(), bar.len())));
                held.push(bar);
            }
            Err(e) => step(&format!("map device {}", i + 1), Err(e.to_string())),
        }
    }
    step(
        "extra map refused",
        match map_device(SELFTEST_LEN) {
            Err(IvshmemError::AlreadyAttached) => Ok(format!("STATUS_DEVICE_ALREADY_ATTACHED while {} device(s) are held", held.len())),
            Err(e) => Err(format!("wrong error: {e}")),
            Ok(_) => Err("a mapping beyond the device count succeeded".into()),
        },
    );
    for (i, bar) in held.iter().enumerate() {
        step(
            &format!("pattern device {}", i + 1),
            pattern_roundtrip(bar, 0).and_then(|()| pattern_roundtrip(bar, 128 << 20)).map(|()| "4 KiB at 0 and 128 MiB".into()),
        );
    }
    let mapped = held.len();
    if let Some(first) = held.pop() {
        step("header on a device", match Region::attach(first, Side::Guest) {
            Ok(r) => Ok(format!(
                "Region::attach ok, {} lane(s): {:?}",
                r.lanes().len(),
                r.lanes().iter().map(|l| (l.id, l.size)).collect::<Vec<_>>()
            )),
            // A fresh backing file has no header until a host viewer has created the region.
            Err(winbridge_shm::RegionError::BadMagic) => Ok("no host header yet (BadMagic): expected on a fresh backing file".to_string()),
            Err(e) => Err(format!("Region::attach failed: {e:?}")),
        });
        // The mapping moved into the region and was dropped with it.
    }
    drop(held);
    // Everything released: every device maps again in this process.
    let again: Vec<_> = (0..mapped).map(|_| map_device(SELFTEST_LEN)).collect();
    step(
        "remap after drop",
        match again.iter().find_map(|r| r.as_ref().err()) {
            None => Ok(format!("{} device(s) mapped again", again.len())),
            Some(e) => Err(e.to_string()),
        },
    );
    drop(again);
    println!("{}", if failed == 0 { "SELFTEST OK" } else { "SELFTEST FAILED" });
    failed.min(1)
}
