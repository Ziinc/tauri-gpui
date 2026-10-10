//! Link stubs for the three OpenGL (CGL) functions GPUI's `core-video`
//! dependency references through `io-surface`.
//!
//! `IOSurface::bind_to_gl_texture` calls them, and the object file holding
//! it is linked into every iOS app because it also holds functions GPUI does
//! use. iOS has no OpenGL framework, so the app fails to link with
//! "Undefined symbols: _CGLGetCurrentContext ...". Nothing calls
//! `bind_to_gl_texture` on iOS; the stubs only satisfy the linker and abort
//! if that ever changes.

use std::ffi::{c_char, c_void};

#[unsafe(no_mangle)]
extern "C" fn CGLGetCurrentContext() -> *mut c_void {
    unreachable_gl("CGLGetCurrentContext")
}

#[unsafe(no_mangle)]
extern "C" fn CGLErrorString(_error: i32) -> *const c_char {
    unreachable_gl("CGLErrorString")
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
extern "C" fn CGLTexImageIOSurface2D(
    _context: *mut c_void,
    _target: u32,
    _internal_format: u32,
    _width: i32,
    _height: i32,
    _format: u32,
    _ty: u32,
    _surface: *mut c_void,
    _plane: u32,
) -> i32 {
    unreachable_gl("CGLTexImageIOSurface2D")
}

fn unreachable_gl(name: &str) -> ! {
    log::error!("tauri-plugin-gpui: {name} called, but iOS has no OpenGL");
    std::process::abort()
}
