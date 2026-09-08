use std::cell::RefCell;
use std::os::fd::AsFd;
use std::rc::Rc;

use anyhow::{ensure, Context as _};
use pipewire::spa::sys::{spa_chunk, SPA_CHUNK_FLAG_NONE};
use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::fence::EGLFence;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::reexports::rustix;
use smithay::utils::{Physical, Rectangle, Scale, Size, Transform};

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
mod tests {
    use super::*;
    use pipewire::spa::sys::SPA_CHUNK_FLAG_CORRUPTED;

    #[test]
    fn egl_deferred_readback_retains_destination_and_pixels() {
        use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
        use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
        use smithay::backend::renderer::element::Kind;
        use std::io::Read;
        use std::time::Duration;

        let mut renderer = unsafe {
            let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
            GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
        };
        let mut staging = StagingTexture::default();
        let native = EGLFence::supports_importing(renderer.egl_context().display());
        eprintln!("SHM test native fence support: {native}");
        for (dimensions, color, expected) in [
            ((16, 8), [1.0, 0.0, 0.0, 1.0], [0, 0, 255, 255]),
            ((7, 13), [0.0, 1.0, 0.0, 1.0], [0, 255, 0, 255]),
        ] {
            let buffer = allocate_shmbuf(Size::from(dimensions)).unwrap();
            let solid = SolidColorBuffer::new((dimensions.0 as f64, dimensions.1 as f64), color);
            let element =
                SolidColorRenderElement::from_buffer(&solid, (0.0, 0.0), 1.0, Kind::Unspecified);
            let (pending, fd) = render_to_shmbuf(
                &mut renderer,
                &mut staging,
                &buffer,
                crate::render_helpers::ReadbackFrame {
                    size: Size::from((dimensions.0 as i32, dimensions.1 as i32)),
                    scale: Scale::from(1.0),
                    transform: Transform::Normal,
                    fourcc: Fourcc::Argb8888,
                    elements: &[element],
                },
            )
            .unwrap();
            assert_eq!(
                fd.is_some(),
                native,
                "native fence silently fell back to synchronous readback"
            );
            drop(buffer);
            if let Some(fd) = fd {
                let mut event_loop = calloop::EventLoop::<bool>::try_new().unwrap();
                event_loop
                    .handle()
                    .insert_source(
                        calloop::generic::Generic::new(
                            fd,
                            calloop::Interest::READ,
                            calloop::Mode::OneShot,
                        ),
                        |_, _, ready| {
                            *ready = true;
                            Ok(calloop::PostAction::Remove)
                        },
                    )
                    .unwrap();
                let mut ready = false;
                event_loop
                    .dispatch(Duration::from_secs(5), &mut ready)
                    .unwrap();
                assert!(ready, "GPU readback fence did not signal");
            }
            let complete = pending.complete(&mut renderer).unwrap();
            let mut file = std::fs::File::from(complete.fd.try_clone().unwrap());
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes.len(), (dimensions.0 * dimensions.1 * 4) as usize);
            assert!(bytes.chunks_exact(4).all(|pixel| pixel == expected));
        }
    }

    #[test]
    fn egl_cancelled_readback_releases_destination_and_rejects_other_context() {
        use crate::render_helpers::solid_color::SolidColorRenderElement;
        use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};

        let make_renderer = || unsafe {
            let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
            GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
        };
        let mut renderer = make_renderer();
        let mut staging = StagingTexture::default();
        for cancel in [true, false] {
            let buffer = allocate_shmbuf(Size::from((4, 4))).unwrap();
            let retained_fd = Rc::downgrade(&buffer.fd);
            let retained_mapping = Rc::downgrade(&buffer.mapping);
            let (pending, fd) = render_to_shmbuf(
                &mut renderer,
                &mut staging,
                &buffer,
                crate::render_helpers::ReadbackFrame {
                    size: Size::from((4, 4)),
                    scale: Scale::from(1.0),
                    transform: Transform::Normal,
                    fourcc: Fourcc::Argb8888,
                    elements: &[] as &[SolidColorRenderElement],
                },
            )
            .unwrap();
            drop(buffer);
            assert!(retained_fd.upgrade().is_some());
            if cancel {
                drop(pending);
            } else {
                let mut other = make_renderer();
                assert!(pending
                    .complete(&mut other)
                    .unwrap_err()
                    .to_string()
                    .contains("renderer changed"));
            }
            drop(fd);
            assert!(retained_fd.upgrade().is_none());
            assert!(retained_mapping.upgrade().is_none());
        }
    }

    #[test]
    fn shm_mapping_survives_buffer_clone_and_reuses_storage() {
        use std::io::Read;
        let buffer = allocate_shmbuf(Size::from((2, 2))).unwrap();
        let retained = buffer.clone();
        drop(buffer);
        assert!(retained.mapping.copy_frame(&[1; 15]).is_err());
        retained.mapping.copy_frame(&[42; 16]).unwrap();
        let read = || {
            let fd = retained.fd.try_clone().unwrap();
            let mut file = std::fs::File::from(fd);
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        };
        assert_eq!(read(), vec![42; 16]);
        retained.mapping.clear();
        assert_eq!(read(), vec![0; 16]);
        retained.mapping.copy_frame(&[7; 16]).unwrap();
        assert_eq!(read(), vec![7; 16]);
    }

    #[test]
    fn shm_layout_uses_spa_representable_dimensions() {
        let layout = ShmLayout::new(Size::from((3840, 2160))).unwrap();
        assert_eq!(layout.stride, 15360);
        assert_eq!(layout.size, 33_177_600);

        assert!(ShmLayout::new(Size::from((536_870_912, 1))).is_err());
        assert!(ShmLayout::new(Size::from((500_000_000, 3))).is_err());
        assert!(ShmLayout::new(Size::from((0, 1))).is_err());
        assert!(ShmLayout::new(Size::from((1, 0))).is_err());
        assert!(layout.matches(Size::from((3840, 2160))));
        assert!(!layout.matches(Size::from((1920, 4320))));
        let mut invalid_size = Size::from((1, 2160));
        invalid_size.w = -1;
        assert!(!layout.matches(invalid_size));
        assert!(!layout.matches(Size::from((i32::MAX, i32::MAX))));
    }

    #[test]
    fn rendered_shm_chunk_covers_the_full_buffer() {
        let layout = ShmLayout::new(Size::from((3840, 2160))).unwrap();
        let mut chunk = spa_chunk {
            offset: 42,
            size: 1,
            stride: -1,
            flags: SPA_CHUNK_FLAG_CORRUPTED as i32,
        };

        mark_shm_chunk_rendered(&mut chunk, layout);

        assert_eq!(chunk.offset, 0);
        assert_eq!(chunk.size, 33_177_600);
        assert_eq!(chunk.stride, 15360);
        assert_eq!(chunk.flags, SPA_CHUNK_FLAG_NONE as i32);
    }
}
