use super::*;
use crate::render_helpers::{
    solid_color::{SolidColorBuffer, SolidColorRenderElement},
    StagingTexture,
};
use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
use smithay::backend::renderer::element::Kind;
use smithay::utils::{Scale, Transform};

#[test]
fn egl_pool_rejects_shared_command_stream_and_unmaps_after_panic() {
    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay).unwrap() };
    let mut renderer =
        unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap() };
    let mut shared = unsafe {
        GlesRenderer::new(EGLContext::new_shared(&display, renderer.egl_context()).unwrap())
            .unwrap()
    };
    assert_eq!(renderer.context_id(), shared.context_id());
    let size = Size::from((7, 13));
    let texture =
        crate::render_helpers::create_texture(&mut renderer, size, Fourcc::Argb8888).unwrap();
    let pending = try_readback(&mut renderer, &texture, size, Fourcc::Argb8888)
        .unwrap()
        .unwrap();
    assert!(pending
        .with_bytes(&mut shared, |_| Ok(()))
        .unwrap_err()
        .to_string()
        .contains("EGL context changed"));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = pending.with_bytes::<()>(&mut renderer, |_| panic!("copy callback failed"));
    }));
    assert!(panicked.is_err());
    pending
        .with_bytes(&mut renderer, |bytes| {
            assert_eq!(bytes.len(), 7 * 13 * 4);
            Ok(())
        })
        .unwrap();
    drop(pending);
    assert!(try_readback(&mut shared, &texture, size, Fourcc::Argb8888)
        .unwrap()
        .is_none());
    assert!(
        try_readback(&mut renderer, &texture, size, Fourcc::Argb8888)
            .unwrap()
            .is_some()
    );
}

#[test]
fn egl_pool_restores_pixel_pack_state() {
    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    };
    let size = Size::from((7, 13));
    let texture =
        crate::render_helpers::create_texture(&mut renderer, size, Fourcc::Argb8888).unwrap();
    let pack = renderer
        .with_context(|gl| unsafe {
            let mut pack = 0;
            gl.GenBuffers(1, &mut pack);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, pack);
            gl.PixelStorei(ffi::PACK_ALIGNMENT, 8);
            gl.PixelStorei(ffi::PACK_ROW_LENGTH, 31);
            gl.PixelStorei(ffi::PACK_SKIP_ROWS, 2);
            gl.PixelStorei(ffi::PACK_SKIP_PIXELS, 3);
            pack
        })
        .unwrap();
    let pending = try_readback(&mut renderer, &texture, size, Fourcc::Argb8888)
        .unwrap()
        .unwrap();
    pending.with_bytes(&mut renderer, |_| Ok(())).unwrap();
    renderer
        .with_context(|gl| unsafe {
            for (parameter, expected) in [
                (ffi::PIXEL_PACK_BUFFER_BINDING, pack as i32),
                (ffi::PACK_ALIGNMENT, 8),
                (ffi::PACK_ROW_LENGTH, 31),
                (ffi::PACK_SKIP_ROWS, 2),
                (ffi::PACK_SKIP_PIXELS, 3),
            ] {
                let mut actual = 0;
                gl.GetIntegerv(parameter, &mut actual);
                assert_eq!(actual, expected);
            }
            assert_eq!(gl.GetError(), ffi::NO_ERROR);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            gl.DeleteBuffers(1, &pack);
        })
        .unwrap();
}

#[test]
fn egl_pool_reuses_allocation_and_does_not_overwrite_in_flight_pixels() {
    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    };
    let mut staging = StagingTexture::default();
    let mut submit = |renderer: &mut GlesRenderer, color, size: (i32, i32)| {
        let solid = SolidColorBuffer::new((size.0 as f64, size.1 as f64), color);
        let element =
            SolidColorRenderElement::from_buffer(&solid, (0.0, 0.0), 1.0, Kind::Unspecified);
        staging
            .render_and_readback(
                renderer,
                Size::from(size),
                Scale::from(1.0),
                Transform::Normal,
                Fourcc::Argb8888,
                &[element],
            )
            .unwrap()
    };
    let first = submit(&mut renderer, [1.0, 0.0, 0.0, 1.0], (16, 8));
    let Readback::Pooled(pending) = &first else {
        panic!("PBO pool was not used")
    };
    let buffer_id = pending.slot.borrow().buffer;
    let second = submit(&mut renderer, [0.0, 1.0, 0.0, 1.0], (16, 8));
    assert!(matches!(second, Readback::SmithayRgba(_)));
    first
        .with_bytes(&mut renderer, |bytes| {
            assert!(bytes.chunks_exact(4).all(|p| p == [0, 0, 255, 255]));
            Ok(())
        })
        .unwrap();
    second
        .with_bytes(&mut renderer, |bytes| {
            assert!(bytes.chunks_exact(4).all(|p| p == [0, 255, 0, 255]));
            Ok(())
        })
        .unwrap();
    drop(first);
    drop(second);
    let resized = submit(&mut renderer, [0.0, 0.0, 1.0, 1.0], (7, 13));
    let Readback::Pooled(pending) = &resized else {
        panic!("pool was not released")
    };
    assert_eq!(pending.slot.borrow().buffer, buffer_id);
    resized
        .with_bytes(&mut renderer, |bytes| {
            assert_eq!(bytes.len(), 7 * 13 * 4);
            assert!(bytes.chunks_exact(4).all(|p| p == [255, 0, 0, 255]));
            Ok(())
        })
        .unwrap();
    drop(resized);
    let cancelled = submit(&mut renderer, [1.0, 0.0, 0.0, 1.0], (16, 8));
    drop(cancelled);
    let after_cancel = submit(&mut renderer, [0.0, 1.0, 0.0, 1.0], (16, 8));
    assert!(matches!(after_cancel, Readback::Pooled(_)));
    after_cancel
        .with_bytes(&mut renderer, |bytes| {
            assert!(bytes.chunks_exact(4).all(|p| p == [0, 255, 0, 255]));
            Ok(())
        })
        .unwrap();
}
