//! Plugin editor windows: a plain AppKit window with a layer-backed content view
//! (composited by Core Animation on Metal) that the plugin draws its own UI into.

use std::ffi::c_void;

use daw_host::Hosted;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

pub struct EditorWindow {
    window: Retained<NSWindow>,
}

impl EditorWindow {
    /// Opens `hosted`'s editor in a new window titled `title`.
    pub fn open(hosted: &mut dyn Hosted, title: &str) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("editors open on the main thread")?;
        let rect = NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(400.0, 300.0));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        // SAFETY: a fresh window on the main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // We hold the window; closing must only hide it until the editor is gone.
        // SAFETY: set before the window is ever closed.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(title));
        let view = window.contentView().ok_or("window has no content view")?;
        view.setWantsLayer(true);
        let parent = Retained::as_ptr(&view).cast_mut().cast::<c_void>();
        // SAFETY: `view` lives as long as `window`, which outlives the editor:
        // `close` takes the editor down before the window goes.
        let (w, h) = unsafe { hosted.open_editor(parent) }.map_err(|e| format!("{title}: {e}"))?;
        let this = Self { window };
        this.resize((w, h));
        this.window.center();
        this.window.makeKeyAndOrderFront(None);
        Ok(this)
    }

    /// False once the user closed the window.
    pub fn is_open(&self) -> bool {
        // Minimized or app-hidden windows are not visible but still open.
        let hidden = MainThreadMarker::new()
            .is_some_and(|mtm| NSApplication::sharedApplication(mtm).isHidden());
        hidden || self.window.isVisible() || self.window.isMiniaturized()
    }

    pub fn front(&self) {
        self.window.makeKeyAndOrderFront(None);
    }

    pub fn resize(&self, (w, h): (f64, f64)) {
        self.window
            .setContentSize(NSSize::new(w.clamp(100.0, 4000.0), h.clamp(60.0, 4000.0)));
    }

    /// Takes the editor out of the window, then the window down.
    pub fn close(self, hosted: &mut dyn Hosted) {
        hosted.close_editor();
        self.window.close();
    }
}
