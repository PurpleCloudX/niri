use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesError, GlesMapping, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Renderer};
use smithay::utils::{Physical, Scale, Size, Transform};

use super::{copy_framebuffer, create_texture, render_elements};

/// Reusable GPU staging texture for readback paths such as PipeWire SHM.
#[derive(Debug, Default)]
pub struct StagingTexture {
    texture: Option<GlesTexture>,
    size: Option<(
        Size<i32, Physical>,
        Fourcc,
        smithay::backend::renderer::ContextId<GlesTexture>,
    )>,
}

impl StagingTexture {
    pub fn render_and_download(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
        transform: Transform,
        fourcc: Fourcc,
        elements: impl Iterator<Item = impl RenderElement<GlesRenderer>>,
    ) -> anyhow::Result<GlesMapping> {
        let texture = self.get_or_create(renderer, size, fourcc)?;
        let mut target = renderer
            .bind(texture)
            .context("error binding staging texture")?;
        let _sync = render_elements(renderer, &mut target, size, scale, transform, elements)?;
        copy_framebuffer(renderer, &target, fourcc).context("error reading staging texture")
    }

    pub fn get_or_create(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        fourcc: Fourcc,
    ) -> Result<&mut GlesTexture, GlesError> {
        let key = (size, fourcc, renderer.context_id());
        if self.size.as_ref() != Some(&key) {
            self.texture = Some(create_texture(renderer, size, fourcc)?);
            self.size = Some(key);
        }
        Ok(self.texture.as_mut().expect("staging texture was created"))
    }
}
