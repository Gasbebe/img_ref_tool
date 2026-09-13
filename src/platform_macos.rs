#![cfg(target_os = "macos")]

//! Best-effort support for dragging images straight out of a browser
//! (Chrome/Safari/Pinterest place raw TIFF/PNG bytes on the drag pasteboard
//! rather than a file path). winit only registers the window for the legacy
//! `NSFilenamesPboardType`, so plain browser image drags never reach it.
//!
//! This installs a transparent overlay view on top of winit's content view,
//! registered only for image pasteboard types. Local file drags from Finder
//! keep working unchanged: AppKit falls back to the view underneath when the
//! frontmost view doesn't accept the dragged types.

use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSDragOperation, NSDraggingDestination, NSDraggingInfo,
    NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSView,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSObjectProtocol, NSPoint, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::app::BrowserDropQueue;

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = BrowserDropQueue]
    struct DropOverlayView;

    unsafe impl NSObjectProtocol for DropOverlayView {}

    unsafe impl NSDraggingDestination for DropOverlayView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            drag_operation_for(sender)
        }

        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            drag_operation_for(sender)
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare_for_drag_operation(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            readable_image_bytes(sender).is_some()
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(
            &self,
            sender: &ProtocolObject<dyn NSDraggingInfo>,
        ) -> objc2::runtime::Bool {
            let Some(bytes) = readable_image_bytes(sender) else {
                return false.into();
            };
            if let Ok(mut queue) = self.ivars().lock() {
                queue.push(bytes);
            }
            true.into()
        }
    }
);

impl DropOverlayView {
    fn new(mtm: MainThreadMarker, queue: BrowserDropQueue) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(queue);
        let zero_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        unsafe { msg_send![super(this), initWithFrame: zero_rect] }
    }
}

fn drag_operation_for(sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
    if readable_image_bytes(sender).is_some() {
        NSDragOperation::Copy
    } else {
        NSDragOperation::None
    }
}

fn readable_image_bytes(sender: &ProtocolObject<dyn NSDraggingInfo>) -> Option<Vec<u8>> {
    let pasteboard = sender.draggingPasteboard();
    // Prefer PNG (lossless, decodes cheaply); fall back to the TIFF
    // representation AppKit/browsers commonly synthesize for image data.
    for pasteboard_type in [unsafe { NSPasteboardTypePNG }, unsafe { NSPasteboardTypeTIFF }] {
        if let Some(data) = pasteboard.dataForType(pasteboard_type) {
            return Some(data.to_vec());
        }
    }
    None
}

/// Installs the overlay on the window eframe just created.
///
/// Returns `None` if the window handle isn't AppKit-based or this isn't
/// running on the main thread (should not happen in practice).
pub fn install(cc: &eframe::CreationContext<'_>) -> Option<BrowserDropQueue> {
    let RawWindowHandle::AppKit(handle) = cc.window_handle().ok()?.as_raw() else {
        return None;
    };
    let mtm = MainThreadMarker::new()?;
    // SAFETY: eframe/winit hand us a pointer to a live NSView owned by the
    // window; retaining it keeps it alive for as long as we hold `Retained`.
    let content_view: Retained<NSView> =
        unsafe { Retained::retain(handle.ns_view.as_ptr().cast()) }?;

    let queue: BrowserDropQueue = Arc::new(Mutex::new(Vec::new()));
    let overlay = DropOverlayView::new(mtm, queue.clone());
    overlay.setFrame(content_view.bounds());
    overlay.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let dragged_types =
        NSArray::from_slice(&[unsafe { NSPasteboardTypePNG }, unsafe { NSPasteboardTypeTIFF }]);
    overlay.registerForDraggedTypes(&dragged_types);
    content_view.addSubview(&overlay);

    Some(queue)
}
