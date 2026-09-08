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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
    use smithay::backend::egl::native::EGLSurfacelessDisplay;
    use smithay::backend::egl::{EGLContext, EGLDisplay};
    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::ExportMem;

    #[test]
    fn egl_staging_reuses_texture_and_reads_fresh_pixels() {
        let mut renderer = unsafe {
            let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
            GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
        };
        let mut staging = StagingTexture::default();
        let size = Size::from((16, 8));
        let mut ids = Vec::new();
        for (color, expected) in [
            ([1.0, 0.0, 0.0, 1.0], [0, 0, 255, 255]),
            ([0.0, 1.0, 0.0, 1.0], [0, 255, 0, 255]),
        ] {
            let buffer = SolidColorBuffer::new((16.0, 8.0), color);
            let element =
                SolidColorRenderElement::from_buffer(&buffer, (0.0, 0.0), 1.0, Kind::Unspecified);
            let mapping = staging
                .render_and_download(
                    &mut renderer,
                    size,
                    Scale::from(1.0),
                    Transform::Normal,
                    Fourcc::Argb8888,
                    std::iter::once(element),
                )
                .unwrap();
            let pixels = renderer.map_texture(&mapping).unwrap();
            assert_eq!(pixels.len(), 16 * 8 * 4);
            assert!(pixels.chunks_exact(4).all(|pixel| pixel == expected));
            ids.push(
                staging
                    .get_or_create(&mut renderer, size, Fourcc::Argb8888)
                    .unwrap()
                    .tex_id(),
            );
        }
        assert_eq!(ids[0], ids[1]);
        let resized = staging
            .get_or_create(&mut renderer, Size::from((8, 8)), Fourcc::Argb8888)
            .unwrap()
            .tex_id();
        assert_ne!(resized, ids[0]);
        let changed_format = staging
            .get_or_create(&mut renderer, Size::from((8, 8)), Fourcc::Xrgb8888)
            .unwrap()
            .tex_id();
        assert_ne!(changed_format, resized);
        let old_context = renderer.context_id();
        let mut other = unsafe {
            let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
            GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
        };
        assert_ne!(old_context, other.context_id());
        staging
            .get_or_create(&mut other, size, Fourcc::Argb8888)
            .unwrap();
        assert_eq!(staging.size.as_ref().unwrap().2, other.context_id());
    }
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
