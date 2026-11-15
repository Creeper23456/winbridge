// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows side: enumerate the ivshmem device interfaces, talk to the driver with
//! `NtDeviceIoControlFile` (so the exact NTSTATUS is visible) and map the BAR.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::time::Duration;

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW,
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SPDRP_LOCATION_INFORMATION, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    SetKernelObjectSecurity, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::dev::{Map, Mode};

const GUID_IVSHMEM: GUID = GUID { data1: 0xdf57_6976, data2: 0x569d, data3: 0x4672, data4: [0x95, 0xa0, 0xf5, 0x7e, 0x4e, 0xa0, 0xb2, 0x10] };

/// CTL_CODE(FILE_DEVICE_UNKNOWN, func, METHOD_BUFFERED, FILE_ANY_ACCESS)
const fn ctl(func: u32) -> u32 {
    (0x22 << 16) | (func << 2)
}
pub const IOCTL_PEERID: u32 = ctl(0x800);
pub const IOCTL_SIZE: u32 = ctl(0x801);
pub const IOCTL_MMAP: u32 = ctl(0x802);
pub const IOCTL_RELEASE: u32 = ctl(0x803);

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

/// NTSTATUS as `0xC0000038 (STATUS_DEVICE_ALREADY_ATTACHED)`.
pub fn nt_name(s: i32) -> String {
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
        _ => "?",
    };
    format!("0x{:08X} ({name})", s as u32)
}

pub struct Handle(pub HANDLE);

// SAFETY: a kernel handle may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: owned handle.
        unsafe { CloseHandle(self.0) };
    }
}

/// Synchronous ioctl; returns (NTSTATUS, bytes returned).
pub fn ioctl(h: HANDLE, code: u32, input: &mut [u8], output: &mut [u8]) -> (i32, usize) {
    let mut iosb = IoStatusBlock { status: 0, information: 0 };
    // SAFETY: buffers are valid for the given lengths; the handle is synchronous so the call completes inline.
    let st = unsafe {
        NtDeviceIoControlFile(
            h,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut iosb,
            code,
            input.as_mut_ptr() as *mut c_void,
            input.len() as u32,
            output.as_mut_ptr() as *mut c_void,
            output.len() as u32,
        )
    };
    (st, iosb.information)
}

#[derive(Clone)]
pub struct Iface {
    pub path: String,
    pub instance: String,
    pub location: String,
}

fn wide_to_string(p: *const u16) -> String {
    let mut v = Vec::new();
    let mut i = 0;
    // SAFETY: caller passes a NUL-terminated buffer.
    unsafe {
        while *p.add(i) != 0 {
            v.push(*p.add(i));
            i += 1;
        }
    }
    String::from_utf16_lossy(&v)
}

/// Every present device interface of the ivshmem driver.
pub fn interfaces() -> Result<Vec<Iface>, String> {
    let mut out = Vec::new();
    // SAFETY: plain SetupAPI enumeration with correctly sized structures and buffers.
    unsafe {
        let set = SetupDiGetClassDevsW(&GUID_IVSHMEM, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set as isize == -1 {
            return Err(format!("SetupDiGetClassDevs failed: {}", GetLastError()));
        }
        let mut idx = 0;
        loop {
            let mut did: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            did.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, null(), &GUID_IVSHMEM, idx, &mut did) == 0 {
                break;
            }
            idx += 1;
            let mut need = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &did, null_mut(), 0, &mut need, null_mut());
            let mut buf = vec![0u64; (need as usize + 7) / 8 + 1];
            let det = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*det).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &did, det, need, null_mut(), &mut info) == 0 {
                let e = GetLastError();
                SetupDiDestroyDeviceInfoList(set);
                return Err(format!("SetupDiGetDeviceInterfaceDetail failed: {e}"));
            }
            let path = wide_to_string(std::ptr::addr_of!((*det).DevicePath) as *const u16);
            let mut wb = [0u16; 512];
            let mut req = 0u32;
            let instance = if SetupDiGetDeviceInstanceIdW(set, &info, wb.as_mut_ptr(), wb.len() as u32, &mut req) != 0 {
                wide_to_string(wb.as_ptr())
            } else {
                String::from("?")
            };
            let mut lb = [0u8; 1024];
            let location = if SetupDiGetDeviceRegistryPropertyW(
                set,
                &info,
                SPDRP_LOCATION_INFORMATION,
                null_mut(),
                lb.as_mut_ptr(),
                lb.len() as u32,
                &mut req,
            ) != 0
            {
                wide_to_string(lb.as_ptr() as *const u16)
            } else {
                String::from("?")
            };
            out.push(Iface { path, instance, location });
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    Ok(out)
}

pub struct Dev {
    pub iface: Iface,
    h: Arc<Handle>,
}

struct MapOwner(Arc<Handle>);

impl Drop for MapOwner {
    fn drop(&mut self) {
        let _ = ioctl(self.0 .0, IOCTL_RELEASE, &mut [], &mut []);
    }
}

/// Decoded `IVSHMEM_MMAP` output.
pub struct MmapOut {
    pub peer: u16,
    pub size: u64,
    pub ptr: u64,
    pub vectors: u16,
}

impl MmapOut {
    pub fn parse(b: &[u8; 32]) -> Self {
        Self {
            peer: u16::from_le_bytes([b[0], b[1]]),
            size: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            ptr: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            vectors: u16::from_le_bytes([b[24], b[25]]),
        }
    }
}

impl Dev {
    /// Open the interface with read/write access; `Err` is the Win32 error code.
    pub fn open(iface: Iface) -> Result<Dev, u32> {
        Self::open_access(iface, GENERIC_READ | GENERIC_WRITE)
    }

    /// Open with an explicit desired access (0 asks for no rights at all); `Err` is the Win32 error code.
    pub fn open_access(iface: Iface, access: u32) -> Result<Dev, u32> {
        let wpath: Vec<u16> = iface.path.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated path.
        let h = unsafe {
            CreateFileW(
                wpath.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: no preconditions.
            return Err(unsafe { GetLastError() });
        }
        Ok(Dev { iface, h: Arc::new(Handle(h)) })
    }

    /// Owner, group and DACL of the device object as an SDDL string.
    pub fn sddl(&self) -> String {
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: valid handle and out pointer; the returned descriptor is freed with LocalFree below.
        unsafe {
            let r = GetSecurityInfo(
                self.h.0,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut sd,
            );
            if r != 0 {
                return format!("GetSecurityInfo failed: Win32 error {r}");
            }
            let mut s: *mut u16 = null_mut();
            let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut s,
                null_mut(),
            );
            let out = if ok != 0 { wide_to_string(s) } else { format!("ConvertSecurityDescriptor failed: Win32 error {}", GetLastError()) };
            if !s.is_null() {
                LocalFree(s as _);
            }
            LocalFree(sd as _);
            out
        }
    }

    /// Replace the DACL of the device object with the one in `sddl` (needs a handle opened with WRITE_DAC).
    pub fn set_dacl(&self, sddl: &str) -> Result<(), u32> {
        let w: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: NUL-terminated SDDL, valid out pointer; the descriptor is freed with LocalFree.
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) == 0 {
                return Err(GetLastError());
            }
            let ok = SetKernelObjectSecurity(self.h.0, DACL_SECURITY_INFORMATION, sd);
            let err = GetLastError();
            LocalFree(sd as _);
            if ok == 0 {
                return Err(err);
            }
        }
        Ok(())
    }

    pub fn handle(&self) -> HANDLE {
        self.h.0
    }

    pub fn label(&self) -> String {
        self.iface.location.clone()
    }

    pub fn describe(&self) -> String {
        let size = self.size().map(|s| format!("{} MiB", s >> 20)).unwrap_or_else(|e| nt_name(e));
        let peer = self.peer().map(|p| p.to_string()).unwrap_or_else(|e| nt_name(e));
        format!("{} | {} | size {size} | peer {peer}", self.iface.location, self.iface.instance)
    }

    pub fn size(&self) -> Result<u64, i32> {
        let mut o = [0u8; 8];
        let (st, _) = ioctl(self.h.0, IOCTL_SIZE, &mut [], &mut o);
        if st < 0 {
            Err(st)
        } else {
            Ok(u64::from_le_bytes(o))
        }
    }

    pub fn peer(&self) -> Result<u16, i32> {
        let mut o = [0u8; 2];
        let (st, _) = ioctl(self.h.0, IOCTL_PEERID, &mut [], &mut o);
        if st < 0 {
            Err(st)
        } else {
            Ok(u16::from_le_bytes(o))
        }
    }

    /// REQUEST_MMAP without taking ownership; returns the status and the decoded output.
    pub fn request_mmap(&self, mode: u8) -> (i32, MmapOut) {
        let mut out = [0u8; 32];
        let (st, _) = ioctl(self.h.0, IOCTL_MMAP, &mut [mode], &mut out);
        (st, MmapOut::parse(&out))
    }

    pub fn release_mmap(&self) -> i32 {
        ioctl(self.h.0, IOCTL_RELEASE, &mut [], &mut []).0
    }

    pub fn map(&self, mode: Mode) -> Result<Map, String> {
        let (st, o) = self.request_mmap(mode as u8);
        if st < 0 {
            return Err(format!("REQUEST_MMAP({}): {}", mode.name(), nt_name(st)));
        }
        Ok(Map::new(o.ptr as *mut u8, o.size as usize, Box::new(MapOwner(self.h.clone()))))
    }
}

pub fn list(files: &[String]) -> Result<Vec<Dev>, String> {
    if !files.is_empty() {
        return Err("--file is only for the Unix loopback self-test".into());
    }
    let mut v = Vec::new();
    for i in interfaces()? {
        let path = i.path.clone();
        v.push(Dev::open(i).map_err(|e| format!("open {path}: Win32 error {e}"))?);
    }
    if v.is_empty() {
        return Err("no ivshmem device interface present".into());
    }
    Ok(v)
}

pub fn cpu_time() -> Duration {
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    // SAFETY: valid out pointers.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    Duration::from_nanos((t(k) + t(u)) * 100)
}
