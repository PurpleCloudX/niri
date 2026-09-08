use std::cell::RefCell;
use std::os::fd::AsFd;
use std::rc::Rc;

use anyhow::{ensure, Context as _};
use pipewire::spa::sys::{spa_chunk, SPA_CHUNK_FLAG_NONE};
use smithay::backend::egl::fence::EGLFence;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::reexports::rustix;
use smithay::utils::{Physical, Rectangle, Size};
#[cfg(test)]
use smithay::{
    backend::allocator::Fourcc,
    utils::{Scale, Transform},
};

use super::shm_mapping::ShmMapping;
use crate::render_helpers::readback_damage::ContentStamp;
use crate::render_helpers::{ReadbackFrame, StagingReadback, StagingTexture};

const SHM_BYTES_PER_PIXEL: usize = 4;

#[derive(Debug, Clone)]
pub(super) struct Shmbuf {
    pub(super) fd: Rc<rustix::fd::OwnedFd>,
    pub(super) layout: ShmLayout,
    mapping: Rc<ShmMapping>,
    content: Rc<RefCell<Option<ContentStamp>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ShmLayout {
    pub(super) stride: i32,
    pub(super) size: u32,
}

impl ShmLayout {
    pub(super) fn new(size: Size<u32, Physical>) -> anyhow::Result<Self> {
        ensure!(size.w > 0 && size.h > 0, "empty SHM frame");
        let stride = size
            .w
            .checked_mul(SHM_BYTES_PER_PIXEL as u32)
            .context("SHM stride overflows u32")?;
        let buffer_size = stride
            .checked_mul(size.h)
            .context("SHM buffer size overflows u32")?;
        // Smithay GLES readback calculates the byte length using signed i32 arithmetic.
        i32::try_from(buffer_size).context("SHM frame exceeds GLES readback limit")?;

        Ok(Self {
            stride: stride.try_into().context("SHM stride exceeds i32")?,
            size: buffer_size,
        })
    }

    pub(super) fn size_usize(self) -> usize {
        self.size as usize
    }

    pub(super) fn matches(self, size: Size<i32, Physical>) -> bool {
        let (Ok(width), Ok(height)) = (u32::try_from(size.w), u32::try_from(size.h)) else {
            return false;
        };
        Self::new(Size::from((width, height))).is_ok_and(|layout| layout == self)
    }
}

pub(super) fn allocate_shmbuf(size: Size<u32, Physical>) -> anyhow::Result<Shmbuf> {
    let layout = ShmLayout::new(size)?;
    let fd = rustix::fs::memfd_create(
        "shm_buffer",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )
    .context("error creating memfd")?;
    rustix::fs::ftruncate(&fd, layout.size.into()).context("error set size of the fd")?;
    rustix::fs::fcntl_add_seals(
        &fd,
        rustix::fs::SealFlags::SEAL | rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
    )
    .context("error sealing the fd")?;
    let mapping = Rc::new(ShmMapping::new(fd.as_fd(), layout.size_usize())?);
    Ok(Shmbuf {
        fd: fd.into(),
        layout,
        mapping,
        content: Rc::new(RefCell::new(None)),
    })
}

#[derive(Debug)]
pub(super) struct ShmReadback {
    mapping: crate::render_helpers::pbo::Readback,
    buffer: Shmbuf,
    context: ContextId<GlesTexture>,
    stamp: ContentStamp,
    region: Option<Rectangle<i32, Physical>>,
}

impl ShmReadback {
    pub(super) fn complete(self, renderer: &mut GlesRenderer) -> anyhow::Result<Shmbuf> {
        ensure!(
            self.context == renderer.context_id(),
            "SHM readback renderer changed"
        );
        self.mapping.with_bytes(renderer, |bytes| {
            if let Some(region) = self.region {
                self.buffer.mapping.copy_region(
                    bytes,
                    self.buffer.layout.stride as usize,
                    region,
                )?;
            }
            Ok(())
        })?;
        *self.buffer.content.borrow_mut() = Some(self.stamp);
        Ok(self.buffer)
    }
}

pub(super) fn render_to_shmbuf(
    renderer: &mut GlesRenderer,
    staging: &mut StagingTexture,
    buffer: &Shmbuf,
    frame: ReadbackFrame<'_, impl RenderElement<GlesRenderer>>,
) -> anyhow::Result<(ShmReadback, Option<rustix::fd::OwnedFd>)> {
    ensure!(
        buffer.layout.matches(frame.size),
        "invalid SHM buffer layout"
    );
    let previous = buffer.content.borrow().clone();
    let StagingReadback {
        mapping,
        stamp,
        region,
    } = staging.render_incremental_readback(renderer, frame, previous.as_ref())?;
    // Fence creation follows ReadPixels in the same GL command stream.
    let fd = if region.is_some() {
        let fence = EGLFence::create(renderer.egl_context().display())
            .context("cannot create SHM completion fence")?;
        renderer.with_context(|gl| unsafe { gl.Flush() })?;
        Some(
            fence
                .export()
                .context("SHM readback requires an exportable completion fence")?,
        )
    } else {
        None
    };
    Ok((
        ShmReadback {
            mapping,
            buffer: buffer.clone(),
            context: renderer.context_id(),
            stamp,
            region,
        },
        fd,
    ))
}

pub(super) fn mark_shm_chunk_rendered(chunk: &mut spa_chunk, layout: ShmLayout) {
    chunk.offset = 0;
    chunk.size = layout.size;
    chunk.stride = layout.stride;
    chunk.flags = SPA_CHUNK_FLAG_NONE as i32;
}

pub(super) fn clear_shmbuf(shmbuf: &Shmbuf) -> anyhow::Result<()> {
    *shmbuf.content.borrow_mut() = None;
    shmbuf.mapping.clear();
    Ok(())
}

#[cfg(test)]
mod incremental_tests;

#[cfg(test)]
mod tests;
