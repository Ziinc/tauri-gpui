//! `Window::render_to_image` for Tauri-hosted windows (`test-support`).
//!
//! The window's own renderer presents to a surface it can't read back, so
//! captures re-render the frame's scene offscreen with GPUI's headless
//! renderer. A scene refers to glyphs and images by their tiles in the sprite
//! atlas, so the window is handed an atlas that inserts every tile into both
//! renderers' atlases. Both allocate tiles the same way, so the tiles match
//! and the scene draws unchanged offscreen; a mismatch disables capture
//! rather than drawing the wrong sprites.

use std::{
    borrow::Cow,
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result, bail};
use gpui::{
    AtlasKey, AtlasTile, DevicePixels, PlatformAtlas, PlatformHeadlessRenderer, Scene, Size,
};
use gpui_wgpu::WgpuHeadlessRenderer;
use image::RgbaImage;

pub(crate) struct Capture {
    renderer: RefCell<WgpuHeadlessRenderer>,
    atlas: Arc<MirroredAtlas>,
}

impl Capture {
    /// `None` when no offscreen device is available; the window then renders
    /// as usual and captures fail with a reason.
    pub fn new(window_atlas: Arc<dyn PlatformAtlas>) -> Option<Self> {
        let renderer = WgpuHeadlessRenderer::new()
            .inspect_err(|error| log::warn!("GPUI window capture unavailable: {error:#}"))
            .ok()?;
        // GPUI hands atlases around as `Arc<dyn PlatformAtlas>`, which is
        // neither `Send` nor `Sync`; this one stays on the main thread too.
        #[allow(clippy::arc_with_non_send_sync)]
        let atlas = Arc::new(MirroredAtlas {
            window: window_atlas,
            offscreen: renderer.sprite_atlas(),
            diverged: AtomicBool::new(false),
        });
        Some(Self {
            renderer: RefCell::new(renderer),
            atlas,
        })
    }

    /// The atlas the window paints with.
    pub fn atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }

    /// Called when the window's renderer cleared its atlas (device loss).
    pub fn atlas_reset(&self) {
        self.atlas.diverged.store(true, Ordering::Relaxed);
    }

    pub fn render(&self, scene: &Scene, size: Size<DevicePixels>) -> Result<RgbaImage> {
        if self.atlas.diverged.load(Ordering::Relaxed) {
            bail!(
                "the window's sprite atlas was reset (GPU device lost), so it can no longer be captured"
            );
        }
        self.renderer
            .borrow_mut()
            .render_scene_to_image(scene, size)
            .context("rendering the window offscreen")
    }
}

/// Inserts every tile into the window's atlas and the offscreen one.
struct MirroredAtlas {
    window: Arc<dyn PlatformAtlas>,
    offscreen: Arc<dyn PlatformAtlas>,
    diverged: AtomicBool,
}

impl PlatformAtlas for MirroredAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        let mut built = None;
        let tile = self.window.get_or_insert_with(key.clone(), &mut || {
            let result = build()?;
            if let Some((size, bytes)) = &result {
                built = Some((*size, bytes.to_vec()));
            }
            Ok(result)
        })?;
        let Some(tile) = tile else {
            return Ok(None);
        };
        if !self.diverged.load(Ordering::Relaxed) {
            // A tile the window already had is in the offscreen atlas too,
            // unless the window's atlas was cleared behind our back.
            let offscreen = self.offscreen.get_or_insert_with(key, &mut || {
                Ok(built
                    .as_ref()
                    .map(|(size, bytes)| (*size, Cow::Borrowed(bytes.as_slice()))))
            });
            if !matches!(offscreen, Ok(Some(mirrored)) if mirrored == tile) {
                log::warn!("GPUI window capture disabled: sprite atlases diverged");
                self.diverged.store(true, Ordering::Relaxed);
            }
        }
        Ok(Some(tile))
    }

    fn remove(&self, key: &AtlasKey) {
        self.window.remove(key);
        self.offscreen.remove(key);
    }
}
