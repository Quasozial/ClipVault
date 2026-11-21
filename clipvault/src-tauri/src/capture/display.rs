//! Monitor enumeration (DXGI) and selection.
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DisplayInfo {
    pub id: String,
    pub output_idx: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub primary: bool,
    pub hdr: bool,
    /// Windows' "SDR content brightness" for this monitor in its own units
    /// (1000 = 80 nits = scRGB 1.0); 0 when unknown. Only meaningful with `hdr`.
    pub sdr_white_level: u32,
}

/// Enumerate the monitors ddagrab can capture, in ddagrab's `output_idx` order.
///
/// FFmpeg's ddagrab (vsrc_ddagrab.c) creates its D3D11 device with no explicit
/// adapter, i.e. on DXGI adapter 0, and opens `IDXGIAdapter::EnumOutputs(output_idx)`
/// on that adapter. We therefore enumerate the outputs of adapter 0 only; monitors
/// driven by another adapter cannot be captured with `output_idx` alone.
#[cfg(windows)]
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, IDXGIFactory1, IDXGIOutput6,
    };
    use windows::core::Interface;

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let adapter = unsafe { factory.EnumAdapters1(0)? };
    let mut out = Vec::new();
    for i in 0u32.. {
        let output = match unsafe { adapter.EnumOutputs(i) } {
            Ok(o) => o,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        let desc = unsafe { output.cast::<IDXGIOutput6>()?.GetDesc1()? };
        let len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
        let id = String::from_utf16_lossy(&desc.DeviceName[..len]);
        let r = desc.DesktopCoordinates;
        // DesktopCoordinates are DPI-virtualized in DPI-unaware processes (a 3840x2160
        // monitor at 150% reads as 2560x1440), so prefer the physical current mode,
        // which is what ddagrab actually captures.
        let mode = current_mode(&desc.DeviceName[..len]);
        let (w, h, x, y) = match mode {
            Some(m) => (m.width, m.height, m.x, m.y),
            None => ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32, r.left, r.top),
        };
        let sdr_white_level = sdr_white_level(&id).unwrap_or(0);
        out.push(DisplayInfo {
            id,
            output_idx: i,
            width: w,
            height: h,
            refresh_hz: mode.map(|m| m.refresh_hz).filter(|&hz| hz > 1).unwrap_or(60),
            primary: x == 0 && y == 0,
            hdr: desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
            sdr_white_level,
        });
    }
    Ok(out)
}

/// SDR white level of the monitor whose GDI name is `device_name` (DisplayConfig
/// `DISPLAYCONFIG_SDR_WHITE_LEVEL`: 1000 = 80 nits).
#[cfg(windows)]
fn sdr_white_level(device_name: &str) -> Option<u32> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;

    let (mut np, mut nm) = (0u32, 0u32);
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) } != ERROR_SUCCESS {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    let rc = unsafe {
        QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    for p in &paths[..np as usize] {
        let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        src.header.adapterId = p.sourceInfo.adapterId;
        src.header.id = p.sourceInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut src.header) } != 0 {
            continue;
        }
        let len = src.viewGdiDeviceName.iter().position(|&c| c == 0).unwrap_or(src.viewGdiDeviceName.len());
        if !String::from_utf16_lossy(&src.viewGdiDeviceName[..len]).eq_ignore_ascii_case(device_name) {
            continue;
        }
        let mut w = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
        w.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
        w.header.size = std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
        w.header.adapterId = p.targetInfo.adapterId;
        w.header.id = p.targetInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut w.header) } != 0 {
            return None;
        }
        return Some(w.SDRWhiteLevel);
    }
    None
}

#[cfg(windows)]
#[derive(Clone, Copy)]
struct Mode {
    width: u32,
    height: u32,
    refresh_hz: u32,
    x: i32,
    y: i32,
}

/// Physical current display mode of `device_name` (e.g. `\\.\DISPLAY8`).
#[cfg(windows)]
fn current_mode(device_name: &[u16]) -> Option<Mode> {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    use windows::core::PCWSTR;

    let mut name: Vec<u16> = device_name.to_vec();
    name.push(0);
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) };
    if !ok.as_bool() || dm.dmPelsWidth == 0 || dm.dmPelsHeight == 0 {
        return None;
    }
    // SAFETY: for display devices DEVMODEW's union holds the display variant (dmPosition).
    let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
    Some(Mode {
        width: dm.dmPelsWidth,
        height: dm.dmPelsHeight,
        // 0 and 1 mean "hardware default" per the DEVMODE docs; caller substitutes 60.
        refresh_hz: dm.dmDisplayFrequency,
        x: pos.x,
        y: pos.y,
    })
}

#[cfg(not(windows))]
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    anyhow::bail!("display enumeration is only supported on Windows")
}

/// Device name (e.g. `\\.\DISPLAY8`) of the monitor showing the largest visible,
/// non-minimized top-level window of process `pid`. `None` while the process
/// has no such window (still launching, minimized, alt-tabbed away).
#[cfg(windows)]
pub fn window_monitor(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFOEXW, MONITOR_DEFAULTTONULL};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GW_OWNER,
    };
    use windows::core::BOOL;

    struct Search {
        pid: u32,
        best: Option<(HWND, i64)>,
    }

    unsafe extern "system" fn visit(hwnd: HWND, lp: LPARAM) -> BOOL {
        // SAFETY: lp is the &mut Search passed to EnumWindows below, alive for the call.
        let s = unsafe { &mut *(lp.0 as *mut Search) };
        let mut owner_pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
        if owner_pid != s.pid
            || !unsafe { IsWindowVisible(hwnd) }.as_bool()
            || unsafe { IsIconic(hwnd) }.as_bool()
            || unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid())
        {
            return true.into();
        }
        let mut r = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
            return true.into();
        }
        let area = i64::from((r.right - r.left).max(0)) * i64::from((r.bottom - r.top).max(0));
        if area > 0 && s.best.is_none_or(|(_, a)| area > a) {
            s.best = Some((hwnd, area));
        }
        true.into()
    }

    let mut s = Search { pid, best: None };
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut s as *mut Search as isize)) };
    let (hwnd, _) = s.best?;
    let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) };
    if mon.is_invalid() {
        return None;
    }
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if !unsafe { GetMonitorInfoW(mon, &mut info.monitorInfo) }.as_bool() {
        return None;
    }
    let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
    Some(String::from_utf16_lossy(&info.szDevice[..len]))
}

#[cfg(not(windows))]
pub fn window_monitor(_pid: u32) -> Option<String> {
    None
}

/// Pick the monitor whose `id` equals `wanted`; otherwise the primary (else the
/// first) monitor. `fell_back` is true when `wanted` was given but not found.
/// With no displays at all, returns a placeholder for ddagrab output 0.
pub fn resolve_display(displays: &[DisplayInfo], wanted: Option<&str>) -> (DisplayInfo, bool) {
    if let Some(w) = wanted {
        if let Some(d) = displays.iter().find(|d| d.id.eq_ignore_ascii_case(w)) {
            return (d.clone(), false);
        }
    }
    let fell_back = wanted.is_some();
    let pick = displays.iter().find(|d| d.primary).or_else(|| displays.first()).cloned();
    let pick = pick.unwrap_or_else(|| DisplayInfo {
        id: String::new(),
        output_idx: 0,
        width: 1920,
        height: 1080,
        refresh_hz: 60,
        primary: true,
        hdr: false,
        sdr_white_level: 0,
    });
    (pick, fell_back)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, idx: u32, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: id.into(),
            output_idx: idx,
            width: 2560,
            height: 1440,
            refresh_hz: 164,
            primary,
            hdr: false,
            sdr_white_level: 0,
        }
    }

    fn three() -> Vec<DisplayInfo> {
        vec![d(r"\\.\DISPLAY6", 0, false), d(r"\\.\DISPLAY8", 1, true), d(r"\\.\DISPLAY7", 2, false)]
    }

    #[test]
    fn resolve_picks_wanted() {
        let (got, fell_back) = resolve_display(&three(), Some(r"\\.\DISPLAY7"));
        assert_eq!(got.id, r"\\.\DISPLAY7");
        assert_eq!(got.output_idx, 2);
        assert!(!fell_back);
    }

    #[test]
    fn resolve_falls_back_to_primary_when_missing() {
        let (got, fell_back) = resolve_display(&three(), Some(r"\\.\DISPLAY9"));
        assert_eq!(got.id, r"\\.\DISPLAY8");
        assert!(got.primary);
        assert!(fell_back);
    }

    #[test]
    fn resolve_none_is_primary_without_fallback_flag() {
        let (got, fell_back) = resolve_display(&three(), None);
        assert_eq!(got.id, r"\\.\DISPLAY8");
        assert!(!fell_back);
    }

    #[test]
    fn list_displays_smoke() {
        let ds = list_displays().expect("list_displays");
        eprintln!("{ds:#?}");
        assert!(!ds.is_empty());
        assert_eq!(ds.iter().filter(|d| d.primary).count(), 1, "{ds:?}");
        for x in &ds {
            assert!(x.id.starts_with(r"\\.\DISPLAY"), "{x:?}");
            assert!(x.width > 0 && x.height > 0 && x.refresh_hz > 0, "{x:?}");
        }
        let mut idx: Vec<u32> = ds.iter().map(|d| d.output_idx).collect();
        idx.sort();
        idx.dedup();
        assert_eq!(idx.len(), ds.len(), "output_idx must be unique: {ds:?}");
    }
}
