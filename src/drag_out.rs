//! Native "drag a file out of the plugin window" support, used to drop the MIDI clip straight
//! into the DAW's arrangement or piano roll.

use nih_plug::editor::ParentWindowHandle;
use std::path::PathBuf;

/// A `Send`able copy of the host's parent window handle.
#[derive(Debug, Clone, Copy)]
pub enum RawParent {
    #[allow(dead_code)]
    Win32(isize),
    #[allow(dead_code)]
    AppKit(usize),
    #[allow(dead_code)]
    X11(u32),
}

impl From<&ParentWindowHandle> for RawParent {
    fn from(handle: &ParentWindowHandle) -> Self {
        match *handle {
            ParentWindowHandle::Win32Hwnd(h) => RawParent::Win32(h as isize),
            ParentWindowHandle::AppKitNsView(v) => RawParent::AppKit(v as usize),
            ParentWindowHandle::X11Window(w) => RawParent::X11(w),
        }
    }
}

/// Whether native drag and drop is implemented on this platform.
pub const SUPPORTED: bool = cfg!(any(target_os = "windows", target_os = "macos"));

#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
fn drag_icon_png() -> Vec<u8> {
    // A tiny piano-roll looking icon: dark rounded tile with a few colored note bars
    const W: u32 = 72;
    const H: u32 = 48;
    let mut rgba = vec![0u8; (W * H * 4) as usize];
    let bars = [(6, 30, 8), (24, 50, 18), (40, 66, 12), (14, 38, 28)];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let corner = |cx: u32, cy: u32| {
                let dx = x as f32 - cx as f32;
                let dy = y as f32 - cy as f32;
                dx * dx + dy * dy > 36.0
            };
            let outside = (x < 6 && y < 6 && corner(6, 6))
                || (x >= W - 6 && y < 6 && corner(W - 7, 6))
                || (x < 6 && y >= H - 6 && corner(6, H - 7))
                || (x >= W - 6 && y >= H - 6 && corner(W - 7, H - 7));
            if outside {
                continue;
            }
            let mut px = [28u8, 30, 40, 235];
            for &(x0, x1, y0) in &bars {
                if x >= x0 && x < x1 && y >= y0 && y < y0 + 6 {
                    px = [148, 110, 255, 255];
                }
            }
            rgba[i..i + 4].copy_from_slice(&px);
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, W, H);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = encoder.write_header() {
            let _ = writer.write_image_data(&rgba);
        }
    }
    out
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod native {
    use super::RawParent;
    use raw_window_handle::{
        AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle,
        WindowHandle,
    };
    use std::num::NonZeroIsize;
    use std::path::PathBuf;
    use std::ptr::NonNull;

    struct Handle(RawWindowHandle);

    impl HasWindowHandle for Handle {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            // SAFETY: The host's window outlives the editor, and we only use it while the editor
            //         is open
            Ok(unsafe { WindowHandle::borrow_raw(self.0) })
        }
    }

    fn raw(parent: RawParent) -> Result<RawWindowHandle, String> {
        match parent {
            RawParent::Win32(hwnd) => NonZeroIsize::new(hwnd)
                .map(|h| RawWindowHandle::Win32(Win32WindowHandle::new(h)))
                .ok_or_else(|| "Invalid window handle".to_string()),
            RawParent::AppKit(view) => NonNull::new(view as *mut std::ffi::c_void)
                .map(|v| RawWindowHandle::AppKit(AppKitWindowHandle::new(v)))
                .ok_or_else(|| "Invalid window handle".to_string()),
            RawParent::X11(_) => Err("Drag and drop is not supported on this platform".into()),
        }
    }

    pub fn start_now(parent: RawParent, file: PathBuf) -> Result<(), String> {
        let handle = Handle(raw(parent)?);
        drag::start_drag(
            &handle,
            drag::DragItem::Files(vec![file]),
            drag::Image::Raw(super::drag_icon_png()),
            |_result, _pos| {},
            drag::Options::default(),
        )
        .map_err(|e| format!("Drag failed: {e}"))
    }
}

#[cfg(target_os = "windows")]
mod deferred {
    //! On Windows `DoDragDrop()` runs its own modal message loop. If we called it from within the
    //! egui update callback, baseview would get re-entered from that loop while its window state
    //! is still borrowed, which panics. So the drag is started from a thread timer instead, which
    //! fires from the host's message loop right after the current event has been handled.
    use super::RawParent;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static PENDING: Mutex<Option<(RawParent, PathBuf)>> = Mutex::new(None);

    type TimerProc = unsafe extern "system" fn(isize, u32, usize, u32);

    #[link(name = "user32")]
    extern "system" {
        fn SetTimer(hwnd: isize, id: usize, elapse: u32, func: Option<TimerProc>) -> usize;
        fn KillTimer(hwnd: isize, id: usize) -> i32;
    }

    unsafe extern "system" fn on_timer(_hwnd: isize, _msg: u32, id: usize, _time: u32) {
        KillTimer(0, id);
        let pending = PENDING.lock().ok().and_then(|mut p| p.take());
        if let Some((parent, file)) = pending {
            // Panics must not unwind into the host
            let _ = std::panic::catch_unwind(move || super::native::start_now(parent, file));
        }
    }

    pub fn schedule(parent: RawParent, file: PathBuf) -> Result<(), String> {
        if let Ok(mut p) = PENDING.lock() {
            *p = Some((parent, file));
        }
        let id = unsafe { SetTimer(0, 0, 0, Some(on_timer)) };
        if id == 0 {
            Err("Could not start the drag operation".into())
        } else {
            Ok(())
        }
    }
}

/// Starts dragging `file` out of the plugin window. Must be called from the GUI thread while the
/// mouse button is held down.
pub fn start(parent: Option<RawParent>, file: PathBuf) -> Result<(), String> {
    let parent = parent.ok_or("The editor window is not open")?;

    #[cfg(target_os = "windows")]
    return deferred::schedule(parent, file);

    #[cfg(target_os = "macos")]
    return native::start_now(parent, file);

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (parent, file);
        Err("Drag and drop is not available on Linux yet, use “Save .mid” instead".into())
    }
}
