use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesError, GlesMapping, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Color32F, Renderer};
use smithay::utils::{Physical, Rectangle, Scale, Size, Transform};

use super::{copy_framebuffer, create_texture};

/// Reusable GPU staging texture for readback paths such as PipeWire SHM.
#[derive(Debug, Default)]
pub struct StagingTexture {
    texture: Option<GlesTexture>,
    size: Option<(
        Size<i32, Physical>,
        Fourcc,
        smithay::backend::renderer::ContextId<GlesTexture>,
        *const std::ffi::c_void,
    )>,
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
        let target = renderer.bind(self.texture.as_mut().unwrap())?;
        Ok(super::pbo::Readback::Smithay(copy_framebuffer(
            renderer, &target, fourcc,
        )?))
    }

    pub fn render_incremental_readback(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
        transform: Transform,
        fourcc: Fourcc,
        elements: &[impl RenderElement<GlesRenderer>],
        previous: Option<&super::readback_damage::ContentStamp>,
    ) -> anyhow::Result<(
        super::pbo::Readback,
        super::readback_damage::ContentStamp,
        Option<Rectangle<i32, Physical>>,
    )> {
        self.render_texture(renderer, size, scale, transform, fourcc, elements)?;
        let (stamp, region) = self.readback_damage.since(previous, size);
        let Some(region) = region else {
            return Ok((super::pbo::Readback::Unchanged, stamp, None));
        };
        if let Some(readback) = super::pbo::try_readback_region(
            renderer,
            self.texture.as_ref().unwrap(),
            size,
            fourcc,
            region,
        )? {
            return Ok((readback, stamp, Some(region)));
        }
        // Smithay remains the full-frame fallback for unsupported or busy PBOs.
        let target = renderer.bind(self.texture.as_mut().unwrap())?;
        let readback = super::pbo::Readback::Smithay(copy_framebuffer(renderer, &target, fourcc)?);
        Ok((readback, stamp, Some(Rectangle::from_size(size))))
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
        let key = (
            size,
            fourcc,
            renderer.context_id(),
            renderer.egl_context().get_context_handle(),
        );
        if self.size.as_ref() != Some(&key) {
            self.texture = Some(create_texture(renderer, size, fourcc)?);
            self.size = Some(key);
            self.damage = None;
            self.readback_damage.reset();
        }
        Ok(self.texture.as_mut().expect("staging texture was created"))
    }
}
