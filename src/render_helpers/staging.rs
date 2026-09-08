use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesError, GlesMapping, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Color32F, Renderer};
use smithay::utils::{Physical, Rectangle, Scale, Size, Transform};

use super::{copy_framebuffer, create_texture};

#[derive(Debug, PartialEq)]
struct TextureKey {
    size: Size<i32, Physical>,
    format: Fourcc,
    renderer: smithay::backend::renderer::ContextId<GlesTexture>,
    context: *const std::ffi::c_void,
}

pub struct ReadbackFrame<'a, E> {
    pub size: Size<i32, Physical>,
    pub scale: Scale<f64>,
    pub transform: Transform,
    pub fourcc: Fourcc,
    pub elements: &'a [E],
}

pub struct StagingReadback {
    pub mapping: super::pbo::Readback,
    pub stamp: super::readback_damage::ContentStamp,
    pub region: Option<Rectangle<i32, Physical>>,
}

/// Reusable GPU staging texture for readback paths such as PipeWire SHM.
#[derive(Debug, Default)]
pub struct StagingTexture {
    texture: Option<GlesTexture>,
    size: Option<TextureKey>,
    damage: Option<(Scale<f64>, Transform, OutputDamageTracker)>,
    readback_damage: super::readback_damage::ReadbackDamage,
    #[cfg(test)]
    last_damage: Option<Vec<smithay::utils::Rectangle<i32, Physical>>>,
}

#[cfg(test)]
mod tests;

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
        self.render_texture(renderer, size, scale, transform, fourcc, elements)?;
        let target = renderer.bind(self.texture.as_mut().unwrap())?;
        copy_framebuffer(renderer, &target, fourcc).context("error reading staging texture")
    }

    pub fn render_and_readback(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
        transform: Transform,
        fourcc: Fourcc,
        elements: &[impl RenderElement<GlesRenderer>],
    ) -> anyhow::Result<super::pbo::Readback> {
        self.render_texture(renderer, size, scale, transform, fourcc, elements)?;
        if let Some(readback) =
            super::pbo::try_readback(renderer, self.texture.as_ref().unwrap(), size, fourcc)?
        {
            return Ok(readback);
        }
        self.download_readback(renderer, fourcc)
    }

    fn download_readback(
        &mut self,
        renderer: &mut GlesRenderer,
        fourcc: Fourcc,
    ) -> anyhow::Result<super::pbo::Readback> {
        let target = renderer.bind(self.texture.as_mut().unwrap())?;
        if matches!(fourcc, Fourcc::Argb8888 | Fourcc::Xrgb8888) {
            return Ok(super::pbo::Readback::SmithayRgba(copy_framebuffer(
                renderer,
                &target,
                Fourcc::Abgr8888,
            )?));
        }
        Ok(super::pbo::Readback::Smithay(copy_framebuffer(
            renderer, &target, fourcc,
        )?))
    }

    pub fn render_incremental_readback(
        &mut self,
        renderer: &mut GlesRenderer,
        frame: ReadbackFrame<'_, impl RenderElement<GlesRenderer>>,
        previous: Option<&super::readback_damage::ContentStamp>,
    ) -> anyhow::Result<StagingReadback> {
        let ReadbackFrame {
            size,
            scale,
            transform,
            fourcc,
            elements,
        } = frame;
        self.render_texture(renderer, size, scale, transform, fourcc, elements)?;
        let (stamp, region) = self.readback_damage.since(previous, size);
        let Some(region) = region else {
            return Ok(StagingReadback {
                mapping: super::pbo::Readback::Unchanged,
                stamp,
                region: None,
            });
        };
        if let Some(readback) = super::pbo::try_readback_region(
            renderer,
            self.texture.as_ref().unwrap(),
            size,
            fourcc,
            region,
        )? {
            return Ok(StagingReadback {
                mapping: readback,
                stamp,
                region: Some(region),
            });
        }
        // Smithay remains the full-frame fallback for unsupported or busy PBOs.
        let readback = self.download_readback(renderer, fourcc)?;
        Ok(StagingReadback {
            mapping: readback,
            stamp,
            region: Some(Rectangle::from_size(size)),
        })
    }

    fn render_texture(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
        transform: Transform,
        fourcc: Fourcc,
        elements: &[impl RenderElement<GlesRenderer>],
    ) -> anyhow::Result<()> {
        self.get_or_create(renderer, size, fourcc)?;
        if self
            .damage
            .as_ref()
            .is_none_or(|(s, t, _)| *s != scale || *t != transform)
        {
            self.readback_damage.reset();
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
        // Incremental callers now accumulate damage against each destination's content stamp.
        let result = self.damage.as_mut().unwrap().2.render_output(
            renderer,
            &mut target,
            1,
            elements,
            Color32F::TRANSPARENT,
        );
        match result {
            Ok(result) => {
                if transform == Transform::Normal {
                    self.readback_damage
                        .record(result.damage.map(Vec::as_slice).unwrap_or_default());
                } else {
                    // Damage is expressed in transformed output coordinates; stay conservative.
                    self.readback_damage.record(&[Rectangle::from_size(size)]);
                }
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
                self.readback_damage.reset();
                return Err(err).context("error rendering staging texture");
            }
        }
        Ok(())
    }

    pub fn get_or_create(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        fourcc: Fourcc,
    ) -> Result<&mut GlesTexture, GlesError> {
        let key = TextureKey {
            size,
            format: fourcc,
            renderer: renderer.context_id(),
            context: renderer.egl_context().get_context_handle(),
        };
        if self.size.as_ref() != Some(&key) {
            let storage_format = if matches!(fourcc, Fourcc::Argb8888 | Fourcc::Xrgb8888) {
                Fourcc::Abgr8888
            } else {
                fourcc
            };
            self.texture = Some(create_texture(renderer, size, storage_format)?);
            self.size = Some(key);
            self.damage = None;
            self.readback_damage.reset();
        }
        Ok(self.texture.as_mut().expect("staging texture was created"))
    }
}
