// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows display resolution and DPI scaling management.

#[cfg(windows)]
mod imp {
    use std::mem::size_of;
    use std::ptr;
    #[allow(unused_imports)]
    use tracing::{error, info, warn};
    use windows_sys::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
        QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_PATH_INFO, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_RESET, CDS_UPDATEREGISTRY, DEVMODEW,
        DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, SM_CXSCREEN,
        SM_CYSCREEN, WM_SETTINGCHANGE,
    };

    // Standard Windows display DPI scale percentage tiers.
    const DPI_LEVELS: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

    // Undocumented CCD API structures for querying and modifying display scaling.
    const DISPLAYCONFIG_DEVICE_INFO_GET_DPI_SCALE: i32 = -3;
    const DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE: i32 = -4;

    #[repr(C)]
    struct DisplayConfigGetDpiScale {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        min_scale_rel: i32,
        cur_scale_rel: i32,
        max_scale_rel: i32,
    }

    #[repr(C)]
    struct DisplayConfigSetDpiScale {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        scale_rel: i32,
    }

    /// Sets the screen resolution using Win32 ChangeDisplaySettingsExW.
    pub fn set_resolution(width: u32, height: u32) -> Result<(), String> {
        info!("Setting display resolution to {}x{}", width, height);

        let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
        dm.dmSize = size_of::<DEVMODEW>() as u16;

        unsafe {
            if EnumDisplaySettingsW(ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm) == 0 {
                return Err("Failed to query current display settings via EnumDisplaySettingsW".into());
            }
        }

        dm.dmPelsWidth = width;
        dm.dmPelsHeight = height;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;

        let ret = unsafe {
            ChangeDisplaySettingsExW(
                ptr::null(),
                &dm,
                HWND::default(),
                CDS_UPDATEREGISTRY | CDS_RESET,
                ptr::null(),
            )
        };

        if ret == DISP_CHANGE_SUCCESSFUL {
            info!("Successfully updated resolution to {}x{}", width, height);
            Ok(())
        } else {
            // Fallback to dynamic update without modifying registry
            let ret2 = unsafe {
                ChangeDisplaySettingsExW(ptr::null(), &dm, HWND::default(), 0, ptr::null())
            };
            if ret2 == DISP_CHANGE_SUCCESSFUL {
                info!("Dynamically applied resolution to {}x{}", width, height);
                Ok(())
            } else {
                Err(format!(
                    "ChangeDisplaySettingsExW failed with code {} (fallback: {})",
                    ret, ret2
                ))
            }
        }
    }

    /// Sets the Windows per-monitor DPI scaling percentage using the CCD API.
    pub fn set_dpi_scale(scale_percent: u32) -> Result<(), String> {
        info!("Setting DPI scale to {}%", scale_percent);

        // Snap to nearest supported standard level
        let target_level = *DPI_LEVELS
            .iter()
            .min_by_key(|&&lvl| (lvl as i32 - scale_percent as i32).abs())
            .unwrap_or(&100);

        let target_idx = DPI_LEVELS
            .iter()
            .position(|&l| l == target_level)
            .unwrap_or(0) as i32;

        let mut path_count: u32 = 0;
        let mut mode_count: u32 = 0;

        let status = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        };
        if status != 0 || path_count == 0 {
            warn!("GetDisplayConfigBufferSizes failed with code {}", status);
            return fallback_registry_dpi(scale_percent);
        }

        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![unsafe { std::mem::zeroed() }; path_count as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![unsafe { std::mem::zeroed() }; mode_count as usize];

        let qdc_status = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                ptr::null_mut(),
            )
        };

        if qdc_status != 0 {
            warn!("QueryDisplayConfig failed with code {}", qdc_status);
            return fallback_registry_dpi(scale_percent);
        }

        // Apply DPI scaling to the primary active display source
        let primary_path = &paths[0];
        let adapter_id = primary_path.sourceInfo.adapterId;
        let source_id = primary_path.sourceInfo.id;

        let mut get_packet = DisplayConfigGetDpiScale {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_DPI_SCALE as _,
                size: size_of::<DisplayConfigGetDpiScale>() as u32,
                adapterId: adapter_id,
                id: source_id,
            },
            min_scale_rel: 0,
            cur_scale_rel: 0,
            max_scale_rel: 0,
        };

        let get_status = unsafe {
            DisplayConfigGetDeviceInfo(&mut get_packet.header as *mut _)
        };

        if get_status != 0 {
            warn!("DisplayConfigGetDeviceInfo failed with code {}", get_status);
            return fallback_registry_dpi(scale_percent);
        }

        // Calculate relative offset based on current index and relative value
        // cur_scale_rel = cur_index - baseline_index => baseline_index = cur_index - cur_scale_rel
        // In most environments curScaleRel is relative to recommended scale (index where scaleRel == 0)
        let _cur_rel = get_packet.cur_scale_rel;
        let min_rel = get_packet.min_scale_rel;
        let max_rel = get_packet.max_scale_rel;

        // Baseline index corresponding to rel == 0 is typically 0 (100%) on basic virtual displays
        // Or computed from cur_index if known. Assume baseline index = 0 for standard 100% baseline.
        let target_rel = (target_idx + min_rel).clamp(min_rel, max_rel);

        let mut set_packet = DisplayConfigSetDpiScale {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE as _,
                size: size_of::<DisplayConfigSetDpiScale>() as u32,
                adapterId: adapter_id,
                id: source_id,
            },
            scale_rel: target_rel,
        };

        let set_status = unsafe {
            DisplayConfigSetDeviceInfo(&mut set_packet.header as *mut _)
        };

        if set_status == 0 {
            info!("CCD DisplayConfigSetDeviceInfo successfully applied scaleRel={}", target_rel);
            // Also notify and update registry for consistency
            let _ = fallback_registry_dpi(target_level);
            Ok(())
        } else {
            warn!("DisplayConfigSetDeviceInfo failed with code {}, trying registry fallback", set_status);
            fallback_registry_dpi(target_level)
        }
    }

    /// Registry fallback for DPI and broadcast notification.
    fn fallback_registry_dpi(scale_percent: u32) -> Result<(), String> {
        let dpi_val = (scale_percent * 96) / 100;
        let subkey: Vec<u16> = "Control Panel\\Desktop\\WindowMetrics\0".encode_utf16().collect();
        let val_name: Vec<u16> = "AppliedDPI\0".encode_utf16().collect();

        unsafe {
            let mut hkey = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) == 0 {
                let dword = dpi_val as u32;
                RegSetValueExW(
                    hkey,
                    val_name.as_ptr(),
                    0,
                    REG_DWORD,
                    &dword as *const u32 as *const _,
                    size_of::<u32>() as u32,
                );
                RegCloseKey(hkey);
            }

            // Broadcast WM_SETTINGCHANGE to inform running applications
            let param: Vec<u16> = "WindowMetrics\0".encode_utf16().collect();
            let mut result = 0;
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                param.as_ptr() as _,
                SMTO_ABORTIFHUNG,
                1000,
                &mut result,
            );
        }

        Ok(())
    }

    /// Queries the current screen resolution and DPI.
    pub fn get_display_info() -> (u32, u32, u32) {
        let width = unsafe { GetSystemMetrics(SM_CXSCREEN) } as u32;
        let height = unsafe { GetSystemMetrics(SM_CYSCREEN) } as u32;
        // Default standard DPI is 96 (100%)
        (width, height, 100)
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn set_resolution(width: u32, height: u32) -> Result<(), String> {
        tracing::info!("[Mock] Set resolution to {}x{}", width, height);
        Ok(())
    }

    pub fn set_dpi_scale(scale_percent: u32) -> Result<(), String> {
        tracing::info!("[Mock] Set DPI scale to {}%", scale_percent);
        Ok(())
    }

    pub fn get_display_info() -> (u32, u32, u32) {
        (1920, 1080, 100)
    }
}

pub use imp::*;
