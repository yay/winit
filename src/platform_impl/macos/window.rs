#![allow(clippy::unnecessary_cast)]

use std::cell::Cell;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::Sel;
use objc2::{declare_class, msg_send, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{NSResponder, NSScreen, NSWindow};
use objc2_foundation::{MainThreadBound, MainThreadMarker, NSObject, NSPoint, NSRect};

use super::event_loop::ActiveEventLoop;
use super::window_delegate::WindowDelegate;
use crate::error::OsError as RootOsError;
use crate::window::WindowAttributes;

pub(crate) struct Window {
    window: MainThreadBound<Retained<WinitWindow>>,
    /// The window only keeps a weak reference to this, so we must keep it around here.
    delegate: MainThreadBound<Retained<WindowDelegate>>,
}

impl Drop for Window {
    fn drop(&mut self) {
        self.window.get_on_main(|window| autoreleasepool(|_| window.close()))
    }
}

impl Window {
    pub(crate) fn new(
        window_target: &ActiveEventLoop,
        attributes: WindowAttributes,
    ) -> Result<Self, RootOsError> {
        let mtm = window_target.mtm;
        let delegate = autoreleasepool(|_| {
            WindowDelegate::new(window_target.app_delegate(), attributes, mtm)
        })?;
        Ok(Window {
            window: MainThreadBound::new(delegate.window().retain(), mtm),
            delegate: MainThreadBound::new(delegate, mtm),
        })
    }

    pub(crate) fn maybe_queue_on_main(&self, f: impl FnOnce(&WindowDelegate) + Send + 'static) {
        // For now, don't actually do queuing, since it may be less predictable
        self.maybe_wait_on_main(f)
    }

    pub(crate) fn maybe_wait_on_main<R: Send>(
        &self,
        f: impl FnOnce(&WindowDelegate) -> R + Send,
    ) -> R {
        self.delegate.get_on_main(|delegate| f(delegate))
    }

    #[cfg(feature = "rwh_06")]
    #[inline]
    pub(crate) fn raw_window_handle_rwh_06(
        &self,
    ) -> Result<rwh_06::RawWindowHandle, rwh_06::HandleError> {
        if let Some(mtm) = MainThreadMarker::new() {
            Ok(self.delegate.get(mtm).raw_window_handle_rwh_06())
        } else {
            Err(rwh_06::HandleError::Unavailable)
        }
    }

    #[cfg(feature = "rwh_06")]
    #[inline]
    pub(crate) fn raw_display_handle_rwh_06(
        &self,
    ) -> Result<rwh_06::RawDisplayHandle, rwh_06::HandleError> {
        Ok(rwh_06::RawDisplayHandle::AppKit(rwh_06::AppKitDisplayHandle::new()))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub usize);

impl WindowId {
    pub const fn dummy() -> Self {
        Self(0)
    }
}

impl From<WindowId> for u64 {
    fn from(window_id: WindowId) -> Self {
        window_id.0 as u64
    }
}

impl From<u64> for WindowId {
    fn from(raw_id: u64) -> Self {
        Self(raw_id as usize)
    }
}

/// Native state scoped to this window, including manually driven pointer gestures.
#[derive(Debug, Default)]
pub struct WinitWindowState {
    app_controlled_drag: Cell<bool>,
    /// AppKit frame adjustment callbacks can recursively invoke one another.
    in_frame_adjustment: Cell<bool>,
}

/// Clears the recursion flag even if a forwarded adjustment unwinds.
struct FrameAdjustmentGuard<'a>(&'a Cell<bool>);

impl Drop for FrameAdjustmentGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

declare_class!(
    #[derive(Debug)]
    pub struct WinitWindow;

    unsafe impl ClassType for WinitWindow {
        #[inherits(NSResponder, NSObject)]
        type Super = NSWindow;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "WinitWindow";
    }

    impl DeclaredClass for WinitWindow {
        type Ivars = WinitWindowState;
    }

    unsafe impl WinitWindow {
        /// During an application-owned drag, the caller controls the frame across display bounds.
        #[method(constrainFrameRect:toScreen:)]
        fn constrain_frame_rect(&self, frame: NSRect, screen: Option<&NSScreen>) -> NSRect {
            if self.app_controlled_drag() {
                return frame;
            }
            unsafe { msg_send![super(self), constrainFrameRect: frame toScreen: screen] }
        }

        // AppKit also adjusts frames asynchronously through private callbacks that can bypass
        // constrainFrameRect:. Suppress those only while the application owns this window's drag,
        // following Chromium's NativeWidgetMacNSWindow workaround. Keep the exact native ABI:
        // anchorIfNeeded is a nullable CGPoint pointer, not a BOOL.
        #[method(_setFrame:fromAdjustmentToScreen:anchorIfNeeded:animate:)]
        fn set_frame_from_adjustment(
            &self,
            frame: NSRect,
            screen: Option<&NSScreen>,
            anchor: *const NSPoint,
            animate: bool,
        ) {
            let selector = sel!(_setFrame:fromAdjustmentToScreen:anchorIfNeeded:animate:);
            let Some(_guard) = self.begin_frame_adjustment(selector, frame) else {
                return;
            };
            unsafe {
                let _: () = msg_send![super(self), _setFrame: frame,
                    fromAdjustmentToScreen: screen, anchorIfNeeded: anchor, animate: animate];
            }
        }

        #[method(_setFrameAfterMove:)]
        fn set_frame_after_move(&self, frame: NSRect) {
            let Some(_guard) = self.begin_frame_adjustment(sel!(_setFrameAfterMove:), frame) else {
                return;
            };
            unsafe { let _: () = msg_send![super(self), _setFrameAfterMove: frame]; }
        }

        /// Do not advertise private callbacks on macOS versions whose NSWindow lacks them.
        #[method(respondsToSelector:)]
        fn responds_to_selector(&self, selector: Sel) -> bool {
            if selector == sel!(_setFrame:fromAdjustmentToScreen:anchorIfNeeded:animate:)
                || selector == sel!(_setFrameAfterMove:)
            {
                Self::supports_frame_adjustment(selector)
            } else {
                unsafe { msg_send![super(self), respondsToSelector: selector] }
            }
        }

        #[method(canBecomeMainWindow)]
        fn can_become_main_window(&self) -> bool {
            trace_scope!("canBecomeMainWindow");
            true
        }

        #[method(canBecomeKeyWindow)]
        fn can_become_key_window(&self) -> bool {
            trace_scope!("canBecomeKeyWindow");
            true
        }
    }
);

impl WinitWindow {
    /// Marks ownership of programmatic drag positioning; it does not start a native move loop.
    /// The caller must clear this on release, cancellation, or transfer to another window.
    pub(super) fn set_app_controlled_drag(&self, active: bool) {
        self.ivars().app_controlled_drag.set(active);
    }

    pub(super) fn app_controlled_drag(&self) -> bool {
        self.ivars().app_controlled_drag.get()
    }

    fn supports_frame_adjustment(selector: Sel) -> bool {
        unsafe { msg_send![NSWindow::class(), instancesRespondToSelector: selector] }
    }

    /// Allows a supported, non-recursive native adjustment outside app-controlled dragging.
    /// The returned guard restores normal callback handling on every return path.
    fn begin_frame_adjustment(
        &self,
        selector: Sel,
        proposed: NSRect,
    ) -> Option<FrameAdjustmentGuard<'_>> {
        if self.app_controlled_drag() {
            static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *TRACE.get_or_init(|| {
                std::env::var_os("STEADY_TRACE_TAB_DRAG").is_some_and(|value| value == "1")
            }) {
                tracing::info!(target: "steady_drag_trace", phase = "suppressed_adjustment",
                    window_id = ?self.id(), ?selector, ?proposed, frame = ?self.frame());
            }
            return None;
        }
        if !Self::supports_frame_adjustment(selector)
            || self.ivars().in_frame_adjustment.replace(true)
        {
            return None;
        }
        Some(FrameAdjustmentGuard(&self.ivars().in_frame_adjustment))
    }

    pub(super) fn id(&self) -> WindowId {
        WindowId(self as *const Self as usize)
    }
}
