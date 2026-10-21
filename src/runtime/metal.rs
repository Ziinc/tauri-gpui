//! The shared GPU context on macOS and iOS. `gpui_wgpu` only creates Vulkan
//! and GL instances, neither of which exists on Apple platforms. The
//! renderer reuses the shared context's instance when there is one, so the
//! runtime creates that context on Metal before the first renderer.
//!
//! GPU device-loss recovery still goes through `gpui_wgpu`'s Vulkan/GL
//! instance and will fail here.

use gpui_wgpu::{WgpuContext, wgpu};
use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject},
};

use super::Runtime;
use crate::{GpuiError, platform::window::RawWindow};

impl Runtime {
    /// Creates the shared context on Metal unless one exists already.
    pub(super) fn seed_metal_context(&self, raw: RawWindow) -> Result<(), GpuiError> {
        if self.platform.gpu_context.borrow().is_some() {
            return Ok(());
        }
        // The renderer creates its surfaces without a display handle, so the
        // instance carries the window's.
        let context = context(Box::new(raw))?;
        *self.platform.gpu_context.borrow_mut() = Some(context);
        Ok(())
    }
}

/// A GPU context on a Metal instance whose display handle is `display`. The
/// adapter is probed with a detached `CAMetalLayer` rather than a window's
/// view, because each surface on a view leaves a sublayer behind.
fn context(display: Box<dyn wgpu::wgt::WgpuHasDisplayHandle>) -> Result<WgpuContext, GpuiError> {
    let init = |e: String| GpuiError::RendererInitialization(e);
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        flags: wgpu::InstanceFlags::default(),
        backend_options: wgpu::BackendOptions::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: Some(display),
    });
    let layer = metal_layer().ok_or_else(|| init("no CAMetalLayer".into()))?;
    // SAFETY: `layer` is a live CAMetalLayer and outlives the surface.
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
            Retained::as_ptr(&layer) as *mut std::ffi::c_void,
        ))
    }
    .map_err(|e| init(format!("Failed to create Metal surface: {e}")))?;
    WgpuContext::new(instance, &surface, None).map_err(|e| init(format!("{e:#}")))
}

/// A new, detached `CAMetalLayer`.
fn metal_layer() -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"CAMetalLayer")?;
    // SAFETY: `+new` on a CALayer subclass returns a retained instance.
    unsafe { msg_send![class, new] }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use gpui_wgpu::wgpu;
    use raw_window_handle::{DisplayHandle, HandleError, HasDisplayHandle};

    #[derive(Debug)]
    struct AppKit;

    impl HasDisplayHandle for AppKit {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            Ok(DisplayHandle::appkit())
        }
    }

    #[test]
    fn creates_the_context_on_metal() {
        let context = super::context(Box::new(AppKit)).expect("a Metal context");
        assert_eq!(context.adapter.get_info().backend, wgpu::Backend::Metal);
    }
}
