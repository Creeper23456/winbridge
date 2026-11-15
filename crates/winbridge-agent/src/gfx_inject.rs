// SPDX-License-Identifier: GPL-3.0-or-later

//! Image File Execution Options (IFEO) injection of the graphics hook DLL.
//!
//! The allow-list is user-owned: `%ProgramData%\WinBridge\gfx-apps.txt`, one bare `.exe` name per
//! line. It is created with defaults on first use and re-read about every 5 seconds. For each
//! listed executable, `Debugger = "<agent>" --ifeo-debug` is written under IFEO in both registry
//! views. Windows then starts this agent instead of the target and appends the target's command
//! line. The agent starts the real target with `DEBUG_ONLY_THIS_PROCESS`, runs it to its initial
//! loader breakpoint, loads the hook DLL there, then detaches so the target runs untraced.
//!
//! Safety rails:
//! - Armed only in a VM and outside safe mode. The VM test needs at least 3 of 5 independent
//!   signals (`is_virtual_machine`). The CPUID hypervisor bit alone does not count, because
//!   Hyper-V and VBS set it on bare metal.
//! - Not armed (bare metal, safe mode, or too few signals): every WinBridge redirect is removed,
//!   best-effort. The target still starts under the debugger with `DEBUG_ONLY_THIS_PROCESS`, which
//!   does not consult its IFEO `Debugger` value, and is detached without injection.
//! - A start that runs this agent again (the bypass did not hold) is stopped instead of repeated.
//! - Recovery record: `HKLM\SOFTWARE\WinBridge\Gfx` has one subkey per armed executable, so the
//!   redirects can be removed offline.
//! - Failure before the target exists fails that launch with exit code 1. Failure after that
//!   detaches the debugger, and the target keeps running.
//! - Crash guard: three consecutive crash exits within 3 s of launch disable an executable's
//!   redirect until its line in `gfx-guard.txt` is removed.
//! - No redirect is written while the hook DLL (`winbridge_gfx_hook.dll`) is missing next to the
//!   agent; the watcher removes any existing ones. The hook DLL does not exist yet: today the
//!   injector therefore installs nothing, even with `--enable-gfx`.
//! - `winbridge-agent.exe --uninstall-gfx` removes every redirect this agent wrote. The agent also
//!   removes them when its session ends (logoff or shutdown), if it runs in an interactive session.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use tracing::{error, info, warn};
use windows_sys::Win32::Foundation::{
    CloseHandle, DBG_CONTINUE, DBG_EXCEPTION_NOT_HANDLED, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND,
    ERROR_NO_MORE_ITEMS, ERROR_SHARING_VIOLATION, ERROR_SUCCESS, EXCEPTION_BREAKPOINT, HANDLE,
    HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, NTSTATUS, WPARAM,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::Diagnostics::Debug::{
    ContinueDebugEvent, DebugActiveProcessStop, DebugSetProcessKillOnExit, WaitForDebugEvent,
    WriteProcessMemory, CREATE_PROCESS_DEBUG_EVENT, DEBUG_EVENT, EXCEPTION_DEBUG_EVENT,
    EXIT_PROCESS_DEBUG_EVENT, LOAD_DLL_DEBUG_EVENT,
};
use windows_sys::Win32::System::Environment::GetCommandLineW;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    KEY_WOW64_32KEY, KEY_WOW64_64KEY, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetSystemFirmwareTable;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeProcess, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject,
    DEBUG_ONLY_THIS_PROCESS, INFINITE, LPTHREAD_START_ROUTINE, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics,
    RegisterClassExW, MSG, SM_CLEANBOOT, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSEXW,
};

/// Serialises IFEO registry edits and session-end cleanup, including the unarmed removal in
/// `armed_hook`. Never held while a target runs.
static LAUNCH_LOCK: Mutex<()> = Mutex::new(());
/// Set once the session is ending. The watcher must not re-arm redirects after this.
static SESSION_ENDING: AtomicBool = AtomicBool::new(false);

const IFEO_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options";
/// Recovery record: one empty subkey per executable this agent has armed.
const RECORD_KEY: &str = r"SOFTWARE\WinBridge\Gfx";
const PCI_ENUM: &str = r"SYSTEM\CurrentControlSet\Enum\PCI";
const BIOS_KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
/// The host-facing virtio-serial port. Its presence is one VM signal.
const WINBRIDGE_PORT: &str = r"\\.\Global\org.winbridge.0";
/// PCI vendors of physical display adapters (NVIDIA, AMD, Intel).
const GPU_VENDORS: [&str; 3] = ["VEN_10DE", "VEN_1002", "VEN_8086"];
/// Firmware strings that name a virtual machine (matched case-insensitively).
const FIRMWARE_MARKERS: [&str; 5] = ["QEMU", "Standard PC", "SeaBIOS", "EDK II", "OVMF"];
/// Number of VM signals that must fire before the gate opens.
const VM_SIGNALS_REQUIRED: usize = 3;
const SESSION_CLASS: &str = "WinBridgeGfxSession";
const DEBUGGER_VALUE: &str = "Debugger";
const DEBUGGER_FLAG: &str = "--ifeo-debug";
/// Hook DLL injected into each allow-listed process; expected next to the agent executable.
const HOOK_DLL: &str = "winbridge_gfx_hook.dll";
/// Native processes read the 64-bit view; 32-bit (WOW64) processes read `WOW6432Node`.
const REG_VIEWS: [u32; 2] = [KEY_WOW64_64KEY, KEY_WOW64_32KEY];
const KEY_ACCESS: u32 = KEY_READ | KEY_WRITE;

const LIST_FILE: &str = "gfx-apps.txt";
const GUARD_FILE: &str = "gfx-guard.txt";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const CRASH_WINDOW: Duration = Duration::from_secs(3);
const CRASH_LIMIT: u32 = 3;
const DISABLED: &str = "disabled";

const LIST_TEMPLATE: &str = "\
# WinBridge GPU acceleration: executables launched through the graphics hook DLL.
#
# One bare executable file name per line (no path, no wildcards). Lines starting with # are ignored.
# An empty or missing list registers nothing. This file is re-read about every 5 seconds.
#
# Shell processes and the compositor are armed by default.
explorer.exe
SystemSettings.exe
ApplicationFrameHost.exe
ShellExperienceHost.exe
StartMenuExperienceHost.exe
SearchHost.exe
TextInputHost.exe
# WARNING: dwm.exe is the Desktop Window Manager; a hook fault in it can black-screen the session.
dwm.exe
#
# Test executables (commented out):
# vkcube.exe
# dxvk-test.exe
";

/// Starts the watchers that keep IFEO redirects in step with the allow-list and remove them when
/// the session ends. Returns at once.
pub fn start_watcher() {
    if let Err(e) = std::thread::Builder::new()
        .name("gfx-ifeo".into())
        .spawn(watch_loop)
    {
        warn!("graphics hook: cannot start the IFEO watcher: {e}");
    }
    if let Err(e) = std::thread::Builder::new()
        .name("gfx-session".into())
        .spawn(session_loop)
    {
        warn!("graphics hook: cannot start the session-end watcher: {e}");
    }
}

/// `--uninstall-gfx`: removes every redirect this agent registered, in both registry views, and
/// the recovery record.
pub fn uninstall() {
    let _lock = LAUNCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match remove_everything() {
        Ok(n) => info!("graphics hook: removed {n} IFEO redirect(s)"),
        Err(e) => error!("graphics hook: cannot remove IFEO redirects: {e}"),
    }
}

/// Target command line when this process was started by IFEO as
/// `<agent> --ifeo-debug <target command line>`.
pub fn ifeo_debug_request() -> Option<String> {
    let cmdline = command_line();
    let (_, rest) = split_first(&cmdline);
    let tail = rest.trim_start().strip_prefix(DEBUGGER_FLAG)?;
    if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
        return None;
    }
    Some(tail.trim_start().to_owned())
}

/// Starts the IFEO-redirected target and returns its exit code. The VM gate is evaluated on every
/// launch, so a redirect left behind on bare metal is removed by the first launch it intercepts.
pub fn run_debugger(target_cmd: &str) -> u32 {
    let (program, _) = split_first(target_cmd);
    let Some(exe) = image_name(program) else {
        warn!("graphics hook: no target executable in {target_cmd:?}");
        return 1;
    };
    let hook = armed_hook(&exe);
    match launch_detached(target_cmd, &exe, hook.as_deref()) {
        Ok(code) => code,
        Err(e) => {
            error!("graphics hook: cannot start {exe}: {e}");
            1
        }
    }
}

/// The hook DLL to inject into `exe`, or `None` when this launch is not armed. A closed gate first
/// removes every redirect, best-effort: the start does not depend on that succeeding.
fn armed_hook(exe: &str) -> Option<PathBuf> {
    if !gate_open() {
        let _lock = LAUNCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = remove_everything() {
            warn!("graphics hook: cannot remove IFEO redirects: {e}");
        }
        return None;
    }
    let inject = should_inject(exe).unwrap_or_else(|e| {
        warn!("graphics hook: cannot decide on injection for {exe} ({e}); starting it without the hook");
        false
    });
    if !inject {
        return None;
    }
    match hook_path() {
        Ok(p) if p.is_file() => Some(p),
        _ => {
            warn!("graphics hook: {HOOK_DLL} not found next to the agent; {exe} runs without it");
            None
        }
    }
}

fn command_line() -> String {
    unsafe {
        let p = GetCommandLineW();
        let mut len = 0;
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(slice::from_raw_parts(p, len))
    }
}

/// Splits a command line into its first token (the program, quoted or bare) and the remainder.
fn split_first(cmd: &str) -> (&str, &str) {
    let cmd = cmd.trim_start();
    if let Some(inner) = cmd.strip_prefix('"') {
        match inner.find('"') {
            Some(end) => (&inner[..end], &inner[end + 1..]),
            None => (inner, ""),
        }
    } else {
        match cmd.find(char::is_whitespace) {
            Some(end) => (&cmd[..end], &cmd[end..]),
            None => (cmd, ""),
        }
    }
}

fn image_name(program: &str) -> Option<String> {
    Path::new(program)
        .file_name()?
        .to_str()
        .map(str::to_ascii_lowercase)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(rc: u32) -> io::Result<()> {
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc as i32))
    }
}

/// The VM gate: outside safe mode and inside a virtual machine. Evaluated on every call.
fn gate_open() -> bool {
    !safe_mode() && is_virtual_machine()
}

fn safe_mode() -> bool {
    unsafe { GetSystemMetrics(SM_CLEANBOOT) != 0 }
}

/// Whether this guest is a virtual machine. Evaluates five independent signals, logs the ones that
/// fired, and requires `VM_SIGNALS_REQUIRED` of them. Read-only, and never cached: the answer must
/// reflect the current boot.
fn is_virtual_machine() -> bool {
    let signals = [
        ("cpuid-hypervisor", cpuid_hypervisor()),
        (
            "firmware",
            smbios_has_marker() || signal("firmware-registry", bios_has_marker()),
        ),
        ("virtio-pci", signal("virtio-pci", pci_has_vendor("VEN_1AF4"))),
        ("winbridge-port", winbridge_port_exists()),
        ("no-display-gpu", signal("no-display-gpu", has_display_gpu().map(|gpu| !gpu))),
    ];
    let fired: Vec<&str> = signals
        .iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| *name)
        .collect();
    let vm = fired.len() >= VM_SIGNALS_REQUIRED;
    info!(
        "graphics hook: {} of {} VM signals fired [{}]: {}",
        fired.len(),
        signals.len(),
        fired.join(", "),
        if vm { "virtual machine" } else { "not a virtual machine" }
    );
    vm
}

/// A signal that cannot be read does not fire. Failing to detect must leave the hook disarmed.
fn signal(name: &str, result: io::Result<bool>) -> bool {
    result.unwrap_or_else(|e| {
        warn!("graphics hook: VM signal {name} unreadable: {e}");
        false
    })
}

/// CPUID.1:ECX bit 31 is set, and the hypervisor vendor leaf reports KVM or TCG (QEMU without KVM).
/// The bit alone does not count: Hyper-V and VBS set it on bare metal.
fn cpuid_hypervisor() -> bool {
    if std::arch::x86_64::__cpuid(1).ecx & 0x8000_0000 == 0 {
        return false;
    }
    let leaf = std::arch::x86_64::__cpuid(0x4000_0000);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&leaf.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&leaf.ecx.to_le_bytes());
    vendor[8..12].copy_from_slice(&leaf.edx.to_le_bytes());
    vendor == *b"KVMKVMKVM\0\0\0" || vendor == *b"TCGTCGTCGTCG"
}

/// The raw SMBIOS table (`'RSMB'`) names a VM firmware. Returns false when it cannot be read.
fn smbios_has_marker() -> bool {
    const RSMB: u32 = u32::from_be_bytes(*b"RSMB");
    let size = unsafe { GetSystemFirmwareTable(RSMB, 0, ptr::null_mut(), 0) };
    if size == 0 {
        return false;
    }
    let mut table = vec![0u8; size as usize];
    let got = unsafe { GetSystemFirmwareTable(RSMB, 0, table.as_mut_ptr().cast(), size) };
    if got == 0 {
        return false;
    }
    table.truncate(got as usize);
    has_vm_marker(&table)
}

/// The BIOS strings the firmware reports under `HARDWARE\DESCRIPTION\System\BIOS`.
fn bios_has_marker() -> io::Result<bool> {
    let Some(key) = open_key(BIOS_KEY, KEY_READ, 0)? else {
        return Ok(false);
    };
    Ok(string_values(&key)?.iter().any(|v| has_vm_marker(v.as_bytes())))
}

fn has_vm_marker(text: &[u8]) -> bool {
    FIRMWARE_MARKERS.iter().any(|m| {
        text.windows(m.len())
            .any(|w| w.eq_ignore_ascii_case(m.as_bytes()))
    })
}

/// Whether any enumerated PCI device has this vendor prefix (for example `VEN_1AF4`).
fn pci_has_vendor(vendor: &str) -> io::Result<bool> {
    let Some(pci) = open_key(PCI_ENUM, KEY_READ, 0)? else {
        return Ok(false);
    };
    Ok(subkey_names(&pci)?.iter().any(|dev| dev.starts_with(vendor)))
}

/// Whether the virtio-serial port exists. Access denied or a sharing violation means it exists and
/// is held by someone else; only a missing device counts as absent.
fn winbridge_port_exists() -> bool {
    let path = wide(WINBRIDGE_PORT);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(handle) };
        return true;
    }
    matches!(
        io::Error::last_os_error().raw_os_error().map(|c| c as u32),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
    )
}

/// Whether a display adapter from a physical GPU vendor is enumerated.
fn has_display_gpu() -> io::Result<bool> {
    let Some(pci) = open_key(PCI_ENUM, KEY_READ, 0)? else {
        return Ok(false);
    };
    let devices = subkey_names(&pci)?;
    for dev in devices
        .iter()
        .filter(|d| GPU_VENDORS.iter().any(|v| d.starts_with(*v)))
    {
        let Some(dev_key) = open_key(&format!(r"{PCI_ENUM}\{dev}"), KEY_READ, 0)? else {
            continue;
        };
        for inst in subkey_names(&dev_key)? {
            let Some(inst_key) = open_key(&format!(r"{PCI_ENUM}\{dev}\{inst}"), KEY_READ, 0)? else {
                continue;
            };
            if read_string(&inst_key, "Class")?.is_some_and(|c| c.eq_ignore_ascii_case("Display")) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn data_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("ProgramData").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "ProgramData is not set")
    })?;
    Ok(PathBuf::from(base).join("WinBridge"))
}

fn list_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join(LIST_FILE))
}

/// Validates one allow-list entry: a bare `.exe` image name with no path or wildcards.
fn validate_image_name(name: &str) -> Result<(), &'static str> {
    if name.contains(['\\', '/', ':']) {
        return Err("path separators are not allowed");
    }
    if name.contains(['*', '?']) {
        return Err("wildcards are not allowed");
    }
    if !name.to_ascii_lowercase().ends_with(".exe") {
        return Err("only .exe image names are allowed");
    }
    Ok(())
}

/// Parses the allow-list. Names are lowercased so comparisons are case-insensitive.
fn parse_list(text: &str) -> BTreeSet<String> {
    let mut apps = BTreeSet::new();
    for (n, raw) in text.lines().enumerate() {
        let name = raw.split('#').next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        match validate_image_name(name) {
            Ok(()) => {
                apps.insert(name.to_ascii_lowercase());
            }
            Err(why) => warn!("graphics hook: {}: line {} ignored: {why}", LIST_FILE, n + 1),
        }
    }
    apps
}

/// Reads the allow-list, creating it with the commented template when absent.
fn read_or_create_list(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, LIST_TEMPLATE)?;
            info!("graphics hook: created {} with the default allow-list", path.display());
            Ok(LIST_TEMPLATE.to_owned())
        }
        Err(e) => Err(e),
    }
}

/// Crash-guard state: `exe=N` counts consecutive early crashes; `exe=disabled` once tripped.
fn read_guard() -> io::Result<BTreeMap<String, String>> {
    let path = data_dir()?.join(GUARD_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect())
}

fn write_guard(state: &BTreeMap<String, String>) -> io::Result<()> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)?;
    let text: String = state.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    std::fs::write(dir.join(GUARD_FILE), text)
}

/// Registry key for one image name under IFEO.
fn image_key(exe: &str) -> io::Result<String> {
    if exe.is_empty() || exe.contains(['\\', '/', ':', '*', '?']) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid image name"));
    }
    Ok(format!(r"{IFEO_KEY}\{exe}"))
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// Opens a key under HKLM with the given access. `None` when the key does not exist.
fn open_key(path: &str, access: u32, view: u32) -> io::Result<Option<Key>> {
    let path = wide(path);
    let mut hkey: HKEY = ptr::null_mut();
    let rc = unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, access | view, &mut hkey)
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(rc)?;
    Ok(Some(Key(hkey)))
}

fn create_key(path: &str, view: u32) -> io::Result<Key> {
    let path = wide(path);
    let mut hkey: HKEY = ptr::null_mut();
    let mut disposition: u32 = 0;
    check(unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_ACCESS | view,
            ptr::null(),
            &mut hkey,
            &mut disposition,
        )
    })?;
    Ok(Key(hkey))
}

/// Names of the direct subkeys of an open key.
fn subkey_names(key: &Key) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for index in 0.. {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                buf.as_mut_ptr(),
                &mut len,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        check(rc)?;
        names.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    Ok(names)
}

/// The `REG_SZ` values of an open key, as text.
fn string_values(key: &Key) -> io::Result<Vec<String>> {
    let mut values = Vec::new();
    for index in 0..256 {
        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut kind: u32 = 0;
        let mut data = vec![0u8; 4096];
        let mut data_len = data.len() as u32;
        let rc = unsafe {
            RegEnumValueW(
                key.0,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                ptr::null(),
                &mut kind,
                data.as_mut_ptr(),
                &mut data_len,
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        check(rc)?;
        if kind != REG_SZ {
            continue;
        }
        let units: Vec<u16> = data[..data_len as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        values.push(String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned());
    }
    Ok(values)
}

/// Reads a `REG_SZ` value of an open key; `None` when absent.
fn read_string(key: &Key, name: &str) -> io::Result<Option<String>> {
    let name = wide(name);
    let mut kind: u32 = 0;
    let mut bytes: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(key.0, name.as_ptr(), ptr::null(), &mut kind, ptr::null_mut(), &mut bytes)
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(rc)?;
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    check(unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null(),
            &mut kind,
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    })?;
    Ok(Some(String::from_utf16_lossy(&buf).trim_end_matches('\0').to_owned()))
}

/// Writes this agent's redirect for `exe` in one registry view.
fn write_debugger(exe: &str, view: u32, command: &str) -> io::Result<()> {
    let key = create_key(&image_key(exe)?, view)?;
    let name = wide(DEBUGGER_VALUE);
    let data = wide(command);
    check(unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * size_of::<u16>()) as u32,
        )
    })
}

/// Whether this agent's redirect for `exe` is present in one view. Read-only.
fn is_ours(exe: &str, view: u32) -> io::Result<bool> {
    let Some(key) = open_key(&image_key(exe)?, KEY_READ, view)? else {
        return Ok(false);
    };
    Ok(read_string(&key, DEBUGGER_VALUE)?.is_some_and(|v| v.contains(DEBUGGER_FLAG)))
}

/// Removes this agent's redirect for `exe` in one view. Returns whether one was removed. Reads
/// first, so a non-elevated caller only needs write access when there is something to remove.
fn remove_ours(exe: &str, view: u32) -> io::Result<bool> {
    if !is_ours(exe, view)? {
        return Ok(false);
    }
    let Some(key) = open_key(&image_key(exe)?, KEY_ACCESS, view)? else {
        return Ok(false);
    };
    let name = wide(DEBUGGER_VALUE);
    check(unsafe { RegDeleteValueW(key.0, name.as_ptr()) })?;
    Ok(true)
}

/// Image names with an IFEO subkey in one view.
fn image_names(view: u32) -> io::Result<Vec<String>> {
    match open_key(IFEO_KEY, KEY_READ, view)? {
        Some(root) => subkey_names(&root),
        None => Ok(Vec::new()),
    }
}

/// Removes every redirect this agent wrote, in both views. Other tools' `Debugger` values stay.
fn remove_all_ours() -> io::Result<usize> {
    let mut removed = 0;
    for view in REG_VIEWS {
        for name in image_names(view)? {
            if remove_ours(&name, view)? {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Writes the recovery record: one subkey per armed executable.
fn write_record(exes: &BTreeSet<String>) -> io::Result<()> {
    for exe in exes {
        create_key(&format!(r"{RECORD_KEY}\{exe}"), 0)?;
    }
    Ok(())
}

/// Deletes the recovery record. Its redirects must already be gone.
fn clear_record() -> io::Result<()> {
    let path = wide(RECORD_KEY);
    let rc = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, path.as_ptr()) };
    if rc == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        check(rc)
    }
}

/// Removes every redirect this agent wrote and the recovery record. Caller holds `LAUNCH_LOCK`.
/// The record is cleared only when every redirect was removed, so an offline recovery can still
/// find the rest.
fn remove_everything() -> io::Result<usize> {
    let removed = remove_all_ours()?;
    clear_record()?;
    Ok(removed)
}

/// Full resync: removes all of this agent's redirects and the record, then records and arms the
/// allow-list minus any executables whose crash guard has tripped.
fn apply_list(path: &Path) -> io::Result<()> {
    let text = read_or_create_list(path)?;
    let tripped: BTreeSet<String> = read_guard()?
        .into_iter()
        .filter(|(_, v)| v == DISABLED)
        .map(|(k, _)| k)
        .collect();
    let wanted: BTreeSet<String> = parse_list(&text)
        .into_iter()
        .filter(|n| !tripped.contains(n))
        .collect();
    let _lock = LAUNCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if SESSION_ENDING.load(Ordering::SeqCst) {
        return Ok(());
    }
    let removed = remove_everything()?;
    write_record(&wanted)?;
    let command = debugger_command()?;
    for exe in &wanted {
        for view in REG_VIEWS {
            if let Err(e) = write_debugger(exe, view, &command) {
                warn!("graphics hook: cannot arm {exe} (view {view:#x}): {e}");
            }
        }
    }
    info!(
        "graphics hook: {} executable(s) armed, {removed} stale redirect(s) removed",
        wanted.len()
    );
    Ok(())
}

fn debugger_command() -> io::Result<String> {
    Ok(format!("\"{}\" {DEBUGGER_FLAG}", std::env::current_exe()?.display()))
}

fn hook_path() -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(HOOK_DLL))
}

fn watch_loop() {
    if !gate_open() {
        info!("graphics hook: not armed (not a VM, or safe mode); removing IFEO redirects");
        let _lock = LAUNCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = remove_everything() {
            warn!("graphics hook: cannot remove IFEO redirects: {e}");
        }
        return;
    }
    let path = match list_path() {
        Ok(p) => p,
        Err(e) => {
            warn!("graphics hook: no allow-list location: {e}");
            return;
        }
    };
    let mut seen: Option<SystemTime> = None;
    let mut dirty = true;
    let mut hook_was_present = true;
    loop {
        // Without the hook DLL a redirect would only put the agent in front of the target (and
        // risk the shell and the compositor) for nothing to inject: keep the registry clean.
        let hook_present = hook_path().is_ok_and(|p| p.is_file());
        if !hook_present {
            if hook_was_present {
                warn!("graphics hook: {HOOK_DLL} not found next to the agent; no IFEO redirects are installed");
                let _lock = LAUNCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = remove_everything() {
                    warn!("graphics hook: cannot remove IFEO redirects: {e}");
                }
            }
            hook_was_present = false;
            dirty = true;
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }
        hook_was_present = true;
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if dirty || mtime != seen {
            match apply_list(&path) {
                Ok(()) => dirty = false,
                Err(e) => {
                    warn!("graphics hook: cannot apply {}: {e}", path.display());
                    dirty = true;
                }
            }
            seen = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Raw `CreateProcessW`. The thread handle is closed here; the caller owns the process handle.
fn create_process(cmd: &str, flags: u32) -> io::Result<PROCESS_INFORMATION> {
    let mut line = wide(cmd);
    let mut si: STARTUPINFOW = unsafe { zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
    let created = unsafe {
        CreateProcessW(
            ptr::null(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            flags,
            ptr::null(),
            ptr::null(),
            &si,
            &mut pi,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { CloseHandle(pi.hThread) };
    Ok(pi)
}

fn exit_code(process: HANDLE) -> u32 {
    let mut code = 1u32;
    if unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
        return 1;
    }
    code
}

/// Whether this target should get the hook: listed, and not tripped by the crash guard.
fn should_inject(exe: &str) -> io::Result<bool> {
    let listed = parse_list(&std::fs::read_to_string(list_path()?)?).contains(exe);
    let tripped = read_guard()?.get(exe).is_some_and(|v| v == DISABLED);
    Ok(listed && !tripped)
}

/// How a new target's run to its initial loader breakpoint ended.
enum Stop {
    /// Stopped at the initial breakpoint. The debug event on thread `tid` is still pending.
    Breakpoint(u32),
    /// The target ended before the breakpoint.
    Exited,
    /// The debugger failed. The target is not stopped.
    Failed,
}

/// Runs the new target's debug events up to its initial loader breakpoint. Image handles the loader
/// passes are closed. Other exceptions go to the target.
fn run_to_breakpoint() -> Stop {
    let mut ev: DEBUG_EVENT = unsafe { zeroed() };
    loop {
        if unsafe { WaitForDebugEvent(&mut ev, INFINITE) } == 0 {
            warn!("graphics hook: debug wait failed: {}", io::Error::last_os_error());
            return Stop::Failed;
        }
        let (ev_pid, ev_tid) = (ev.dwProcessId, ev.dwThreadId);
        let mut status: NTSTATUS = DBG_CONTINUE;
        unsafe {
            match ev.dwDebugEventCode {
                CREATE_PROCESS_DEBUG_EVENT => close_file(ev.u.CreateProcessInfo.hFile),
                LOAD_DLL_DEBUG_EVENT => close_file(ev.u.LoadDll.hFile),
                EXCEPTION_DEBUG_EVENT => {
                    // The first breakpoint is the loader's initial one; the caller injects there.
                    if ev.u.Exception.ExceptionRecord.ExceptionCode == EXCEPTION_BREAKPOINT {
                        return Stop::Breakpoint(ev_tid);
                    }
                    status = DBG_EXCEPTION_NOT_HANDLED;
                }
                EXIT_PROCESS_DEBUG_EVENT => {
                    ContinueDebugEvent(ev_pid, ev_tid, DBG_CONTINUE);
                    return Stop::Exited;
                }
                _ => {}
            }
            ContinueDebugEvent(ev_pid, ev_tid, status);
        }
    }
}

/// Stops debugging `pid`. Kill-on-exit was cleared before the launch, so the target keeps running.
fn detach(pid: u32) {
    if unsafe { DebugActiveProcessStop(pid) } == 0 {
        warn!("graphics hook: cannot detach from pid {pid}: {}", io::Error::last_os_error());
    }
}

/// Whether `process` runs this agent's own image.
fn runs_this_agent(process: HANDLE) -> io::Result<bool> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let image = String::from_utf16_lossy(&buf[..len as usize]);
    Ok(image.eq_ignore_ascii_case(&std::env::current_exe()?.to_string_lossy()))
}

/// Starts the target with `DEBUG_ONLY_THIS_PROCESS`, injects `hook` at its initial loader breakpoint
/// when one is given, and detaches so it runs untraced. A process created this way does not consult
/// its IFEO `Debugger` value, so the start does not re-enter the agent even if a redirect survives.
/// Blocks until the target exits and returns its real exit code. No lock is held while it waits.
fn launch_detached(cmd: &str, exe: &str, hook: Option<&Path>) -> io::Result<u32> {
    // Set before the target exists: if this agent faults, the OS detaches the target instead of
    // killing it.
    unsafe { DebugSetProcessKillOnExit(0) };
    let started = Instant::now();
    let pi = create_process(cmd, DEBUG_ONLY_THIS_PROCESS)?;
    // Backstop for the bypass: a start that is still this agent, or cannot be checked, is stopped.
    if !matches!(runs_this_agent(pi.hProcess), Ok(false)) {
        unsafe {
            TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hProcess);
        }
        return Err(io::Error::other(format!("{exe} re-entered this agent or could not be checked; stopped")));
    }
    match run_to_breakpoint() {
        Stop::Breakpoint(tid) => {
            if let Some(hook) = hook {
                if let Err(e) = inject_hook(pi.hProcess, hook) {
                    warn!("graphics hook: injection into {exe} failed, continuing without it: {e}");
                }
            }
            unsafe { ContinueDebugEvent(pi.dwProcessId, tid, DBG_CONTINUE) };
            detach(pi.dwProcessId);
        }
        Stop::Failed => detach(pi.dwProcessId),
        Stop::Exited => {}
    }
    unsafe { WaitForSingleObject(pi.hProcess, INFINITE) };
    let ran = started.elapsed();
    let code = exit_code(pi.hProcess);
    unsafe { CloseHandle(pi.hProcess) };
    record_exit(exe, code, ran);
    Ok(code)
}

/// Closes the file handle the loader passes with image-load events.
fn close_file(h: HANDLE) {
    if !h.is_null() && h != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(h) };
    }
}

/// Starts `LoadLibraryW(dll)` in the target. Does not wait for it: the target's main thread is
/// stopped at the breakpoint, so waiting here could deadlock on the loader lock.
fn inject_hook(process: HANDLE, dll: &Path) -> io::Result<()> {
    let path = wide(
        dll.to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hook path is not UTF-8"))?,
    );
    let bytes = path.len() * size_of::<u16>();
    let remote = unsafe {
        VirtualAllocEx(process, ptr::null(), bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
    };
    if remote.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut written = 0usize;
    if unsafe { WriteProcessMemory(process, remote, path.as_ptr().cast(), bytes, &mut written) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let kernel32 = wide("kernel32.dll");
    let module = unsafe { GetModuleHandleW(kernel32.as_ptr()) };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }
    // kernel32 maps at the same base in every process of a boot, so this address is valid in the target.
    let load = unsafe { GetProcAddress(module, b"LoadLibraryW\0".as_ptr()) }
        .ok_or_else(|| io::Error::other("LoadLibraryW not found"))?;
    let start: LPTHREAD_START_ROUTINE = unsafe { std::mem::transmute(load) };

    let mut tid = 0u32;
    let thread = unsafe { CreateRemoteThread(process, ptr::null(), 0, start, remote, 0, &mut tid) };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    unsafe { CloseHandle(thread) };
    Ok(())
}

/// Records how a debugged target ended. Three consecutive crashes within 3 s of launch remove its
/// redirect and disable it until the `gfx-guard.txt` line is removed.
fn record_exit(exe: &str, exit_code: u32, ran: Duration) {
    if let Err(e) = update_guard(exe, exit_code, ran) {
        warn!("graphics hook: crash guard state not saved for {exe}: {e}");
    }
}

fn update_guard(exe: &str, exit_code: u32, ran: Duration) -> io::Result<()> {
    let mut state = read_guard()?;
    // NTSTATUS error and warning severities have the high bit set.
    let crashed_early = (exit_code & 0x8000_0000) != 0 && ran < CRASH_WINDOW;
    if !crashed_early {
        state.remove(exe);
    } else {
        let count = state
            .get(exe)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
            + 1;
        if count < CRASH_LIMIT {
            state.insert(exe.to_owned(), count.to_string());
        } else {
            warn!(
                "graphics hook: {exe} crashed {count} times in a row within {}s of launch (exit {exit_code:#010x}); disabling its redirect. Remove its line in {GUARD_FILE} to re-arm.",
                CRASH_WINDOW.as_secs()
            );
            for view in REG_VIEWS {
                if let Err(e) = remove_ours(exe, view) {
                    warn!("graphics hook: cannot remove the {exe} redirect (view {view:#x}): {e}");
                }
            }
            state.insert(exe.to_owned(), DISABLED.to_owned());
        }
    }
    write_guard(&state)
}

/// Runs the session-end watcher: a hidden top-level window that receives WM_ENDSESSION. A
/// message-only window does not receive it.
fn session_loop() {
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let class = wide(SESSION_CLASS);
    let mut wc: WNDCLASSEXW = unsafe { zeroed() };
    wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
    wc.lpfnWndProc = Some(session_proc);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    if unsafe { RegisterClassExW(&wc) } == 0 {
        warn!(
            "graphics hook: cannot register the session-end window class: {}",
            io::Error::last_os_error()
        );
        return;
    }
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if window.is_null() {
        warn!(
            "graphics hook: cannot create the session-end window: {}",
            io::Error::last_os_error()
        );
        return;
    }
    loop {
        let mut msg: MSG = unsafe { zeroed() };
        if unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } <= 0 {
            break;
        }
        unsafe { DispatchMessageW(&msg) };
    }
}

unsafe extern "system" fn session_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            end_session();
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The session is ending: stop re-arming, and remove every redirect and the record.
fn end_session() {
    SESSION_ENDING.store(true, Ordering::SeqCst);
    let _lock = LAUNCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match remove_everything() {
        Ok(n) => info!("graphics hook: session ending; removed {n} IFEO redirect(s)"),
        Err(e) => warn!("graphics hook: session ending; cannot remove IFEO redirects: {e}"),
    }
}
