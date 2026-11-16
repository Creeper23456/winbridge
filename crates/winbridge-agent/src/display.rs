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
        QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
        DISPLAYCONFIG_MODE_INFO_TYPE_TARGET, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_RATIONAL,
        QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES,
        SDC_APPLY, SDC_SAVE_TO_DATABASE, SDC_USE_SUPPLIED_DISPLAY_CONFIG, SDC_VALIDATE,
    };
    /// DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE (not exported by windows-sys): the source/target
    /// mode indices of a path are packed into 16-bit halves.
    const DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE: u32 = 0x8;

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
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
    use windows_sys::Win32::UI::HiDpi::{
        GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        MDT_EFFECTIVE_DPI,
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

    /// Sets the screen resolution. The virtio-gpu driver only accepts a mode after the host has
    /// announced it (QEMU SetUIInfo), so retry for a short while on DISP_CHANGE_BADMODE.
    pub fn set_resolution(width: u32, height: u32) -> Result<(), String> {
        let mut last = String::new();
        for _ in 0..15 {
            match set_resolution_once(width, height) {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Err(last)
    }

    /// The active display paths and their modes, as CCD reports them.
    fn query_active_config() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
        let mut path_count: u32 = 0;
        let mut mode_count: u32 = 0;
        let status = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        };
        if status != 0 || path_count == 0 {
            return Err(format!("GetDisplayConfigBufferSizes failed with code {}", status));
        }

        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
            vec![unsafe { std::mem::zeroed() }; path_count as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            vec![unsafe { std::mem::zeroed() }; mode_count as usize];
        let status = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(format!("QueryDisplayConfig failed with code {}", status));
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        Ok((paths, modes))
    }

    /// The refresh rate of the first active path in millihertz, 0 when it reports none.
    fn path_millihertz(path: &DISPLAYCONFIG_PATH_INFO) -> u32 {
        let rate = path.targetInfo.refreshRate;
        if rate.Denominator == 0 {
            return 0;
        }
        (u64::from(rate.Numerator) * 1000 / u64::from(rate.Denominator)) as u32
    }

    /// Sets the refresh rate of the virtual screen. Ok only when the display configuration reports
    /// the requested rate afterwards. The configuration is validated first, so a driver that does
    /// not offer the rate keeps the mode it has. Two forms are tried: the rate on the path and in
    /// the target's video signal, then on the path alone.
    pub fn set_refresh_rate(millihertz: u32) -> Result<(), String> {
        let (mut paths, modes) = query_active_config()?;
        let before = path_millihertz(&paths[0]);
        if before == millihertz {
            return Ok(());
        }
        let rate = DISPLAYCONFIG_RATIONAL { Numerator: millihertz, Denominator: 1000 };
        paths[0].targetInfo.refreshRate = rate;
        let raw = unsafe { paths[0].targetInfo.Anonymous.modeInfoIdx };
        let idx = if paths[0].flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 {
            raw & 0xFFFF
        } else {
            raw
        } as usize;
        let has_target_mode = idx < modes.len() && modes[idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;

        let mut failures = Vec::new();
        for with_signal in [true, false] {
            if with_signal && !has_target_mode {
                continue;
            }
            let mut attempt = modes.clone();
            if with_signal {
                attempt[idx].Anonymous.targetMode.targetVideoSignalInfo.vSyncFreq = rate;
            }
            let submit = |flags: u32| unsafe {
                SetDisplayConfig(
                    paths.len() as u32,
                    paths.as_ptr(),
                    attempt.len() as u32,
                    attempt.as_ptr(),
                    flags | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
                )
            };
            let form = if with_signal { "path and signal" } else { "path only" };
            let status = submit(SDC_VALIDATE);
            if status != 0 {
                failures.push(format!("{form}: validation code {status}"));
                continue;
            }
            let status = submit(SDC_APPLY | SDC_SAVE_TO_DATABASE);
            if status != 0 {
                failures.push(format!("{form}: apply code {status}"));
                continue;
            }
            let applied = query_active_config().map_or(0, |(p, _)| path_millihertz(&p[0]));
            if applied != millihertz {
                // The driver validated and accepted the configuration but kept its own rate.
                failures.push(format!("{form}: accepted, but the path still reports {applied} mHz"));
                continue;
            }
            info!("SetDisplayConfig applied {} mHz ({})", millihertz, form);
            return Ok(());
        }
        Err(format!(
            "the display driver does not take {} mHz (it runs at {} mHz): {}",
            millihertz,
            before,
            failures.join("; ")
        ))
    }

    /// Applies the size through the CCD API, the way the virtio-win helper does it. Windows only
    /// offers a fixed mode list to ChangeDisplaySettings, while the GPU driver's custom mode
    /// (taken from the host window size) is accepted as a supplied display configuration.
    fn set_resolution_ccd(width: u32, height: u32) -> Result<(), String> {
        let (paths, mut modes) = query_active_config()?;
        let path_count = paths.len() as u32;
        let mode_count = modes.len() as u32;
        let path = &paths[0];
        let raw = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
        let idx = if path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 {
            (raw >> 16) & 0xFFFF
        } else {
            raw
        } as usize;
        if idx >= modes.len() || modes[idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return Err(format!("source mode index {} is not a source mode", idx));
        }
        modes[idx].Anonymous.sourceMode.width = width;
        modes[idx].Anonymous.sourceMode.height = height;

        let status = unsafe {
            SetDisplayConfig(
                path_count,
                paths.as_ptr(),
                mode_count,
                modes.as_ptr(),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
            )
        };
        if status == 0 {
            info!("SetDisplayConfig applied {}x{}", width, height);
            Ok(())
        } else {
            Err(format!("SetDisplayConfig failed with code {}", status))
        }
    }

    /// One attempt using Win32 ChangeDisplaySettingsExW.
    fn set_resolution_once(width: u32, height: u32) -> Result<(), String> {
        match set_resolution_ccd(width, height) {
            Ok(()) => return Ok(()),
            Err(e) => warn!("CCD resolution change failed ({}), trying ChangeDisplaySettingsExW", e),
        }
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

        // Relative values are offsets from the recommended level, which is not necessarily the
        // first one. The recommended index follows from the level that is active right now.
        let cur_rel = get_packet.cur_scale_rel;
        let min_rel = get_packet.min_scale_rel;
        let max_rel = get_packet.max_scale_rel;
        let cur_idx = effective_dpi_percent().map(|p| nearest_level_idx(p));
        let recommended_idx = match cur_idx {
            Some(ci) => ci - cur_rel,
            None => -min_rel,
        };
        let target_rel = (target_idx - recommended_idx).clamp(min_rel, max_rel);
        info!(
            "DPI levels: cur_rel={} min_rel={} max_rel={} recommended_idx={} target_idx={} -> rel={}",
            cur_rel, min_rel, max_rel, recommended_idx, target_idx, target_rel
        );

        let mut rel = target_rel;
        for attempt in 0..2 {
            let mut set_packet = DisplayConfigSetDpiScale {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE as _,
                    size: size_of::<DisplayConfigSetDpiScale>() as u32,
                    adapterId: adapter_id,
                    id: source_id,
                },
                scale_rel: rel,
            };

            let set_status = unsafe { DisplayConfigSetDeviceInfo(&mut set_packet.header as *mut _) };
            if set_status != 0 {
                warn!("DisplayConfigSetDeviceInfo failed with code {}, trying registry fallback", set_status);
                return fallback_registry_dpi(target_level);
            }

            // The change lands asynchronously: wait for the effective DPI to follow.
            for _ in 0..20 {
                if effective_dpi_percent() == Some(target_level) {
                    info!("Effective DPI is now {}%", target_level);
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            // Not there: the reported offset may have been stale. Correct by the observed difference.
            match effective_dpi_percent() {
                Some(effective) if attempt == 0 => {
                    let corrected =
                        (rel + target_idx - nearest_level_idx(effective)).clamp(min_rel, max_rel);
                    if corrected == rel {
                        break;
                    }
                    info!("DPI landed on {}%, retrying with rel={}", effective, corrected);
                    rel = corrected;
                }
                _ => break,
            }
        }
        Err(format!(
            "requested {}% but the effective DPI is {:?}% (Windows limits the scale for this resolution)",
            target_level,
            effective_dpi_percent()
        ))
    }

    /// Makes the agent per-monitor DPI aware so it sees real pixel sizes and DPI values.
    pub fn init_dpi_awareness() {
        if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } == 0 {
            warn!("SetProcessDpiAwarenessContext failed");
        }
    }

    /// Effective DPI of the primary monitor as a scale percentage (96 dpi = 100%).
    fn effective_dpi_percent() -> Option<u32> {
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let hr = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
        if hr == 0 && dpi_x != 0 {
            Some((dpi_x * 100 + 48) / 96)
        } else {
            None
        }
    }

    /// Index in `DPI_LEVELS` closest to a scale percentage.
    fn nearest_level_idx(percent: u32) -> i32 {
        DPI_LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, &l)| (l as i32 - percent as i32).abs())
            .map(|(i, _)| i as i32)
            .unwrap_or(0)
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
        (width, height, effective_dpi_percent().unwrap_or(100))
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn init_dpi_awareness() {}

    pub fn set_resolution(width: u32, height: u32) -> Result<(), String> {
        tracing::info!("[Mock] Set resolution to {}x{}", width, height);
        Ok(())
    }

    pub fn set_dpi_scale(scale_percent: u32) -> Result<(), String> {
        tracing::info!("[Mock] Set DPI scale to {}%", scale_percent);
        Ok(())
    }

    pub fn set_refresh_rate(millihertz: u32) -> Result<(), String> {
        tracing::info!("[Mock] Set refresh rate to {} mHz", millihertz);
        Ok(())
    }

    pub fn get_display_info() -> (u32, u32, u32) {
        (1920, 1080, 100)
    }
}

pub use imp::*;
