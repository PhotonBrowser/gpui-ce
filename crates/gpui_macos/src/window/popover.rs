//! `WindowKind::AnchoredPopup` on macOS, as a native `NSPopover`.
//!
//! The window keeps its own `NSWindow`, which holds its state, delegate and
//! renderer as any other window, but never shows it. Its view is shown in an
//! `NSPopover` anchored to the parent window instead, which draws the system's
//! popover material behind it, animates it in and out, and closes it when the
//! person clicks elsewhere. Queries about where the window is on screen, and
//! whether it is key, follow the view into the popover's window.

use super::*;
use gpui::popup::{PopupGravity, PopupOptions};

/// The popover a window is shown in, and the object it reports to.
pub(super) struct PopoverHost {
    popover: ObjcId,
    delegate: ObjcId,
}

static mut DELEGATE_CLASS: *const AnyClass = ptr::null();

#[ctor]
unsafe fn build_delegate_class() {
    unsafe {
        let mut decl = ClassBuilder::new(
            CString::new("GPUIPopoverDelegate").unwrap().as_c_str(),
            class!(NSObject),
        )
        .unwrap();
        decl.add_ivar::<*mut c_void>(CString::new(WINDOW_STATE_IVAR).unwrap().as_c_str());
        decl.add_method(
            sel!(dealloc),
            dealloc_delegate as unsafe extern "C" fn(_, _),
        );
        decl.add_method(
            sel!(popoverDidShow:),
            popover_did_show as unsafe extern "C" fn(_, _, _),
        );
        decl.add_method(
            sel!(popoverDidClose:),
            popover_did_close as unsafe extern "C" fn(_, _, _),
        );
        // The popover's window reports its key status here, as a window's
        // own delegate would.
        decl.add_method(
            sel!(windowDidBecomeKey:),
            window_did_change_key_status as unsafe extern "C" fn(_, _, _),
        );
        decl.add_method(
            sel!(windowDidResignKey:),
            window_did_change_key_status as unsafe extern "C" fn(_, _, _),
        );
        DELEGATE_CLASS = decl.register();
    }
}

impl PopoverHost {
    /// Shows `window_state`'s view in a popover anchored as `options` asks,
    /// `size` large. `None` when the parent is not an open GPUI window.
    pub(super) unsafe fn show(
        window_state: &Arc<Mutex<MacWindowState>>,
        options: &PopupOptions,
        size: Size<Pixels>,
    ) -> Option<Self> {
        unsafe {
            let parent_view = gpui_view_for(options.parent)?;
            let native_view = window_state.lock().native_view.as_ptr();

            let frame = Objc2NSRect::new(
                Objc2NSPoint::new(0.0, 0.0),
                NSSize::new(size.width.as_f32() as f64, size.height.as_f32() as f64),
            );
            let container: ObjcId = msg_send![class!(NSView), alloc];
            let container: ObjcId = msg_send![container, initWithFrame: frame];
            let _: () = msg_send![native_view, retain];
            let _: () = msg_send![native_view, removeFromSuperview];
            let _: () = msg_send![native_view, setFrame: frame];
            let _: () = msg_send![container, addSubview: native_view];
            let _: () = msg_send![native_view, release];

            let controller: ObjcId = msg_send![class!(NSViewController), new];
            let _: () = msg_send![controller, setView: container];
            let _: () = msg_send![container, release];

            let delegate: ObjcId = msg_send![DELEGATE_CLASS, new];
            set_window_state_ivar(delegate, Arc::into_raw(window_state.clone()) as *mut c_void);

            let popover: ObjcId = msg_send![class!(NSPopover), new];
            // NSPopoverBehaviorTransient: closes on a click elsewhere.
            let _: () = msg_send![popover, setBehavior: 1 as NSInteger];
            let _: () = msg_send![popover, setAnimates: Bool::new(true)];
            let _: () = msg_send![popover, setContentSize: frame.size];
            let _: () = msg_send![popover, setContentViewController: controller];
            let _: () = msg_send![controller, release];
            let _: () = msg_send![popover, setDelegate: delegate];
            if !options.arrow {
                hide_arrow(popover);
            }

            let anchor = anchor_rect(parent_view, options);
            let edge = preferred_edge(options.gravity);
            let _: () = msg_send![
                popover,
                showRelativeToRect: anchor,
                ofView: parent_view,
                preferredEdge: edge
            ];
            // The popover's window exists once shown; take the keyboard there.
            let popover_window: ObjcId = msg_send![native_view, window];
            if !popover_window.is_null() {
                let _: () = msg_send![popover_window, makeFirstResponder: native_view];
            }
            Some(Self { popover, delegate })
        }
    }

    /// Closes the popover without reporting back, as the window goes.
    pub(super) unsafe fn close(self) {
        unsafe {
            stop_observing_key_status(self.delegate);
            let _: () = msg_send![self.popover, setDelegate: NIL];
            let _: () = msg_send![self.popover, close];
            let _: () = msg_send![self.popover, release];
            let _: () = msg_send![self.delegate, release];
        }
    }
}

/// The window the view is shown in: the popover's while it is shown,
/// otherwise the window's own.
pub(super) fn host_window(state: &MacWindowState) -> ObjcId {
    let shown_in: ObjcId = unsafe { msg_send![state.native_view.as_ptr(), window] };
    if shown_in.is_null() {
        state.native_window
    } else {
        shown_in
    }
}

/// How far the view lies from its window's bottom-left corner, which pointer
/// positions are measured from. Zero for a view filling its own window.
pub(super) fn view_origin_in_window(state: &MacWindowState) -> Objc2NSPoint {
    if state.popover.is_none() {
        return Objc2NSPoint::new(0.0, 0.0);
    }
    unsafe {
        msg_send![
            state.native_view.as_ptr(),
            convertPoint: Objc2NSPoint::new(0.0, 0.0),
            toView: NIL
        ]
    }
}

/// The GPUI view of the open window `handle`.
unsafe fn gpui_view_for(handle: AnyWindowHandle) -> Option<ObjcId> {
    unsafe {
        let app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
        let windows: ObjcId = msg_send![app, windows];
        let count: NSUInteger = msg_send![windows, count];
        for index in 0..count {
            let window: ObjcId = msg_send![windows, objectAtIndex: index];
            let is_gpui_window: Bool = msg_send![window, isKindOfClass: &*WINDOW_CLASS];
            let is_gpui_panel: Bool = msg_send![window, isKindOfClass: &*PANEL_CLASS];
            if !(is_gpui_window.as_bool() || is_gpui_panel.as_bool()) {
                continue;
            }
            let state = get_window_state(&*window);
            let state = state.lock();
            if state.handle == handle {
                return Some(state.native_view.as_ptr());
            }
        }
        None
    }
}

/// `options.anchor_rect`, in the parent window's top-left coordinates, as a
/// rectangle in the parent's view, moved by `options.offset`.
unsafe fn anchor_rect(parent_view: ObjcId, options: &PopupOptions) -> Objc2NSRect {
    let bounds: Objc2NSRect = unsafe { msg_send![parent_view, bounds] };
    let flipped: Bool = unsafe { msg_send![parent_view, isFlipped] };
    let rect = options.anchor_rect;
    let x = (rect.origin.x + options.offset.x).as_f32() as f64;
    let top = (rect.origin.y + options.offset.y).as_f32() as f64;
    let (width, height) = (
        rect.size.width.as_f32().max(1.0) as f64,
        rect.size.height.as_f32().max(1.0) as f64,
    );
    let y = if flipped.as_bool() {
        top
    } else {
        bounds.size.height - top - height
    };
    Objc2NSRect::new(Objc2NSPoint::new(x, y), NSSize::new(width, height))
}

/// The side of the anchor the popover opens on, from the way it grows.
fn preferred_edge(gravity: PopupGravity) -> NSUInteger {
    // NSRectEdge: MinX, MinY, MaxX, MaxY, in a view whose y runs up.
    const MIN_X: NSUInteger = 0;
    const MIN_Y: NSUInteger = 1;
    const MAX_X: NSUInteger = 2;
    const MAX_Y: NSUInteger = 3;
    match gravity {
        PopupGravity::Left | PopupGravity::TopLeft | PopupGravity::BottomLeft => MIN_X,
        PopupGravity::Right | PopupGravity::TopRight | PopupGravity::BottomRight => MAX_X,
        PopupGravity::Top => MAX_Y,
        PopupGravity::Bottom | PopupGravity::Center => MIN_Y,
    }
}

/// Hides the popover's arrow, through AppKit's private `shouldHideAnchor`,
/// which AppKit has long honored; a release without it keeps the arrow.
unsafe fn hide_arrow(popover: ObjcId) {
    unsafe {
        let responds: Bool = msg_send![popover, respondsToSelector: sel!(setShouldHideAnchor:)];
        if responds.as_bool() {
            let _: () = msg_send![popover, setShouldHideAnchor: Bool::new(true)];
        }
    }
}

unsafe fn stop_observing_key_status(delegate: ObjcId) {
    unsafe {
        let center: ObjcId = msg_send![class!(NSNotificationCenter), defaultCenter];
        let _: () = msg_send![center, removeObserver: delegate];
    }
}

unsafe extern "C" fn popover_did_show(this: &Objc2Object, _: Sel, _: ObjcId) {
    unsafe {
        let window_state = get_window_state(this);
        let mut lock = window_state.lock();
        let native_view = lock.native_view.as_ptr();
        let popover_window: ObjcId = msg_send![native_view, window];
        if popover_window.is_null() {
            return;
        }
        // Follow the popover window's key status, as the window's own
        // delegate does for it, and take the keyboard there.
        let center: ObjcId = msg_send![class!(NSNotificationCenter), defaultCenter];
        for name in [
            "NSWindowDidBecomeKeyNotification",
            "NSWindowDidResignKeyNotification",
        ] {
            let selector = if name.contains("Become") {
                sel!(windowDidBecomeKey:)
            } else {
                sel!(windowDidResignKey:)
            };
            let name = ns_string(name);
            let _: () = msg_send![
                center,
                addObserver: this as *const Objc2Object as ObjcId,
                selector: selector,
                name: &*name,
                object: popover_window
            ];
        }
        let _: () = msg_send![popover_window, makeFirstResponder: native_view];
        // Shown on a display other than the one it opened on, it draws there.
        lock.start_display_link();
    }
}

unsafe extern "C" fn popover_did_close(this: &Objc2Object, _: Sel, _: ObjcId) {
    unsafe {
        let window_state = get_window_state(this);
        let native_window = {
            let mut lock = window_state.lock();
            lock.stop_display_link();
            lock.native_window
        };
        // Closing the window's own NSWindow runs the close path any window
        // takes, so its owner hears that it closed.
        let _: () = msg_send![native_window, close];
    }
}

unsafe extern "C" fn dealloc_delegate(this: &Objc2Object, _: Sel) {
    unsafe {
        drop_window_state(this);
        let _: () = msg_send![super(this, class!(NSObject)), dealloc];
    }
}
