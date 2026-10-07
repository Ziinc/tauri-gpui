//! Mobile (iOS/Android) support, enabled with the `mobile` Cargo feature.
//!
//! Mobile builds reuse [`gpui-mobile`](https://crates.io/crates/gpui-mobile)
//! instead of reimplementing GPUI's mobile platform layer. The published
//! `gpui-mobile` 0.1 only ships its platform-independent momentum-scrolling
//! engine; its `Platform` implementations are not on crates.io yet. Until
//! they are, [`crate::init`] and [`crate::GpuiWindowExt::attach_gpui`] return
//! [`crate::GpuiError::UnsupportedOperation`] on iOS and Android, while the
//! public attachment API stays identical to desktop.

pub use gpui_mobile;
