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
