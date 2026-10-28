//! Opens the editor of each Apple Audio Unit into an offscreen view and prints its
//! size, without showing a window: a quick check that editor hosting works here.

use std::ffi::c_void;

use daw_host::{Catalog, Format};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSRect, NSSize};

fn main() {
    let mtm = MainThreadMarker::new().expect("run on the main thread");
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(400.0, 300.0));
    let parent = NSView::initWithFrame(NSView::alloc(mtm), rect);
    let ptr = Retained::as_ptr(&parent).cast_mut().cast::<c_void>();
    let mut catalog = Catalog::scan();
    let units: Vec<_> = catalog
        .available
        .iter()
        .filter(|a| a.format == Format::AudioUnit && a.info.id.ends_with(":appl"))
        .cloned()
        .collect();
    for a in units {
        let Ok((mut hosted, _node)) =
            catalog.instantiate(a.format, a.bundle.as_deref(), &a.info.id, 48_000.0, 256)
        else {
            continue;
        };
        // SAFETY: `parent` outlives the editor, which closes right after.
        match unsafe { hosted.open_editor(ptr) } {
            Ok((w, h)) => println!("{:<28} editor {w:.0} x {h:.0}", a.info.name),
            Err(e) => println!("{:<28} {e}", a.info.name),
        }
        hosted.close_editor();
    }
}
