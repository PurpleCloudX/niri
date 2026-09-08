use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesError, GlesMapping, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Color32F, Renderer};
use smithay::utils::{Physical, Scale, Size, Transform};

use super::{copy_framebuffer, create_texture};

/// Reusable GPU staging texture for readback paths such as PipeWire SHM.
#[derive(Debug, Default)]
pub struct StagingTexture {
    texture: Option<GlesTexture>,
    size: Option<(
        Size<i32, Physical>,
        Fourcc,
        smithay::backend::renderer::ContextId<GlesTexture>,
    )>,
    damage: Option<(Scale<f64>, Transform, OutputDamageTracker)>,
    #[cfg(test)]
    last_damage: Option<Vec<smithay::utils::Rectangle<i32, Physical>>>,
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
    fn egl_staging_preserves_unchanged_pixels_and_invalidates_geometry() {
        let mut renderer = unsafe {
            let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
            GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
        };
        let size = Size::from((16, 8));
        let mut staging = StagingTexture::default();
        let background = SolidColorBuffer::new((16.0, 8.0), [1.0, 0.0, 0.0, 1.0]);
        let mut foreground = SolidColorBuffer::new((4.0, 4.0), [0.0, 1.0, 0.0, 1.0]);
        let back =
            SolidColorRenderElement::from_buffer(&background, (0.0, 0.0), 1.0, Kind::Unspecified);
        let front =
            SolidColorRenderElement::from_buffer(&foreground, (0.0, 0.0), 1.0, Kind::Unspecified);
        let mut elements = vec![front, back.clone()];
        for step in 0..5 {
            if step == 2 {
                foreground.set_color([0.0, 0.0, 1.0, 1.0]);
                elements[0] = SolidColorRenderElement::from_buffer(
                    &foreground,
                    (0.0, 0.0),
                    1.0,
                    Kind::Unspecified,
                );
            }
            if step == 4 {
                elements.remove(0);
            }
            let mapping = staging
                .render_and_download(
                    &mut renderer,
                    size,
                    Scale::from(1.0),
                    Transform::Normal,
                    Fourcc::Argb8888,
                    &elements,
                )
                .unwrap();
            let pixels = renderer.map_texture(&mapping).unwrap();
            for y in 0..8 {
                for x in 0..16 {
                    let expected = if step < 4 && x < 4 && y < 4 {
                        if step < 2 {
                            [0, 255, 0, 255]
                        } else {
                            [255, 0, 0, 255]
                        }
                    } else {
                        [0, 0, 255, 255]
                    };
                    assert_eq!(
                        &pixels[(y * 16 + x) * 4..(y * 16 + x + 1) * 4],
                        expected,
                        "step={step} x={x} y={y}"
                    );
                }
            }
            if step == 1 || step == 3 {
                assert!(
                    staging.last_damage.is_none(),
                    "identical scene must skip drawing"
                );
            }
            if step == 2 {
                let area: i32 = staging
                    .last_damage
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|r| r.size.w * r.size.h)
                    .sum();
                assert!(
                    area < size.w * size.h,
                    "changing a small element should not redraw the full frame"
                );
            }
        }
        for (scale, transform) in [
            (Scale::from(0.5), Transform::Normal),
            (Scale::from(1.0), Transform::_180),
        ] {
            let mapping = staging
                .render_and_download(
                    &mut renderer,
                    size,
                    scale,
                    transform,
                    Fourcc::Argb8888,
                    &[back.clone()],
                )
                .unwrap();
            let pixels = renderer.map_texture(&mapping).unwrap().to_vec();
            let reference = crate::render_helpers::render_and_download(
                &mut renderer,
                size,
                scale,
                transform,
                Fourcc::Argb8888,
                std::iter::once(&back),
            )
            .unwrap();
            assert_eq!(pixels, renderer.map_texture(&reference).unwrap());
            assert!(
                staging.last_damage.is_some(),
                "geometry changes must invalidate damage history"
            );
        }
    }

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
                    &[element],
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
        elements: &[impl RenderElement<GlesRenderer>],
    ) -> anyhow::Result<GlesMapping> {
        self.get_or_create(renderer, size, fourcc)?;
        if self
            .damage
            .as_ref()
            .is_none_or(|(s, t, _)| *s != scale || *t != transform)
        {
            self.damage = Some((
                scale,
                transform,
                OutputDamageTracker::new(size, scale, transform),
            ));
        }
        let texture = self.texture.as_mut().unwrap();
        let mut target = renderer
            .bind(texture)
            .context("error binding staging texture")?;
        // The same texture retains the previous frame, so its buffer age is one.
        // Consumers still receive a full readback, independent of their buffer age.
        let result = self.damage.as_mut().unwrap().2.render_output(
            renderer,
            &mut target,
            1,
            elements,
            Color32F::TRANSPARENT,
        );
        match result {
            Ok(result) => {
                #[cfg(test)]
                {
                    self.last_damage = result.damage.cloned();
                }
                #[cfg(not(test))]
                let _ = result;
            }
            Err(err) => {
                // A failed render must never be treated as a valid cached frame.
                self.damage = None;
                return Err(err).context("error rendering staging texture");
            }
        }
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
            self.damage = None;
        }
        Ok(self.texture.as_mut().expect("staging texture was created"))
    }
}
