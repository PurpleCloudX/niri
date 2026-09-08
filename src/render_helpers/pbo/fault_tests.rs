use super::*;
use smithay::backend::egl::{
    get_proc_address, native::EGLSurfacelessDisplay, EGLContext, EGLDisplay,
};

fn renderer() -> GlesRenderer {
    unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    }
}

unsafe extern "system" fn rgba_implementation(parameter: u32, value: *mut i32) {
    match parameter {
        ffi::IMPLEMENTATION_COLOR_READ_FORMAT => value.write(ffi::RGBA as i32),
        ffi::IMPLEMENTATION_COLOR_READ_TYPE => value.write(ffi::UNSIGNED_BYTE as i32),
        _ => {
            let real: unsafe extern "system" fn(u32, *mut i32) =
                std::mem::transmute(get_proc_address("glGetIntegerv"));
            real(parameter, value);
        }
    }
}

unsafe extern "system" fn without_bgra_extension(parameter: u32) -> *const u8 {
    if parameter == ffi::EXTENSIONS {
        return c"".as_ptr().cast();
    }
    let real: unsafe extern "system" fn(u32) -> *const u8 =
        std::mem::transmute(get_proc_address("glGetString"));
    real(parameter)
}

#[test]
fn egl_rgba_only_driver_path_preserves_bgra_pixels() {
    use crate::render_helpers::{
        solid_color::{SolidColorBuffer, SolidColorRenderElement},
        StagingTexture,
    };
    use smithay::{
        backend::renderer::element::Kind,
        utils::{Scale, Transform},
    };
    let mut renderer = renderer();
    let size = Size::from((7, 13));
    let mut staging = StagingTexture::default();
    let solid = SolidColorBuffer::new((7.0, 13.0), [1.0, 0.0, 0.0, 1.0]);
    let element = SolidColorRenderElement::from_buffer(&solid, (0.0, 0.0), 1.0, Kind::Unspecified);
    let reference = staging
        .render_and_download(
            &mut renderer,
            size,
            Scale::from(1.0),
            Transform::Normal,
            Fourcc::Argb8888,
            &[element],
        )
        .unwrap();
    let texture = staging
        .get_or_create(&mut renderer, size, Fourcc::Argb8888)
        .unwrap();
    let slot = Rc::new(RefCell::new(Slot {
        context: renderer.context_id(),
        egl_context: renderer.egl_context().get_context_handle(),
        buffer: 0,
        capacity: 0,
        busy: true,
        disabled: false,
    }));
    let order = renderer
        .with_context(|_| unsafe {
            let gl = ffi::Gles2::load_with(|name| match name {
                "glGetIntegerv" => rgba_implementation as *const std::ffi::c_void,
                "glGetString" => without_bgra_extension as *const std::ffi::c_void,
                _ => get_proc_address(name),
            });
            transfer::read(
                &gl,
                &mut slot.borrow_mut(),
                texture,
                Rectangle::from_size(size),
                364,
            )
            .unwrap()
            .unwrap()
        })
        .unwrap();
    assert!(matches!(order, PixelOrder::Rgba));
    let pending = Readback::Pooled(PooledReadback {
        slot: slot.clone(),
        len: 364,
        order,
    });
    pending
        .with_bytes(&mut renderer, |bytes| {
            assert!(bytes.chunks_exact(4).all(|pixel| pixel == [0, 0, 255, 255]));
            Ok(())
        })
        .unwrap();
    drop(pending);
    drop(reference);
    renderer
        .with_context(|gl| unsafe {
            gl.DeleteBuffers(1, &slot.borrow().buffer);
        })
        .unwrap();
}

unsafe extern "system" fn no_framebuffer(_: i32, output: *mut u32) {
    output.write(0);
}

unsafe extern "system" fn failed_allocation(
    target: u32,
    _: isize,
    data: *const std::ffi::c_void,
    usage: u32,
) {
    let real: unsafe extern "system" fn(u32, isize, *const std::ffi::c_void, u32) =
        std::mem::transmute(get_proc_address("glBufferData"));
    real(target, -1, data, usage);
}

#[test]
fn egl_allocation_failures_restore_state_without_disabling_future_reads() {
    let mut renderer = renderer();
    let size = Size::from((7, 13));
    let texture =
        crate::render_helpers::create_texture(&mut renderer, size, Fourcc::Abgr8888).unwrap();
    let mut slot = Slot {
        context: renderer.context_id(),
        egl_context: renderer.egl_context().get_context_handle(),
        buffer: 0,
        capacity: 0,
        busy: false,
        disabled: false,
    };
    for (symbol, replacement) in [
        (
            "glGenFramebuffers",
            no_framebuffer as *const std::ffi::c_void,
        ),
        ("glBufferData", failed_allocation as *const std::ffi::c_void),
    ] {
        renderer
            .with_context(|gl| unsafe {
                let mut before = [0; 2];
                gl.GetIntegerv(ffi::READ_FRAMEBUFFER_BINDING, &mut before[0]);
                gl.GetIntegerv(ffi::PIXEL_PACK_BUFFER_BINDING, &mut before[1]);
                gl.PixelStorei(ffi::PACK_ALIGNMENT, 8);
                let faulty = ffi::Gles2::load_with(|name| {
                    if name == symbol {
                        replacement
                    } else {
                        get_proc_address(name)
                    }
                });
                assert!(transfer::read(
                    &faulty,
                    &mut slot,
                    &texture,
                    Rectangle::from_size(size),
                    364
                )
                .is_err());
                assert!(!slot.disabled);
                let mut after = [0; 2];
                gl.GetIntegerv(ffi::READ_FRAMEBUFFER_BINDING, &mut after[0]);
                gl.GetIntegerv(ffi::PIXEL_PACK_BUFFER_BINDING, &mut after[1]);
                assert_eq!(after, before);
                let mut alignment = 0;
                gl.GetIntegerv(ffi::PACK_ALIGNMENT, &mut alignment);
                assert_eq!(alignment, 8);
                assert!(
                    transfer::read(gl, &mut slot, &texture, Rectangle::from_size(size), 364)
                        .unwrap()
                        .is_some()
                );
            })
            .unwrap();
    }
    renderer
        .with_context(|gl| unsafe {
            gl.DeleteBuffers(1, &slot.buffer);
        })
        .unwrap();
}

#[test]
fn egl_prior_error_does_not_permanently_disable_readback() {
    let mut renderer = renderer();
    let size = Size::from((7, 13));
    let texture =
        crate::render_helpers::create_texture(&mut renderer, size, Fourcc::Abgr8888).unwrap();
    renderer
        .with_context(|gl| unsafe {
            gl.PixelStorei(u32::MAX, 0);
        })
        .unwrap();
    assert!(
        try_readback(&mut renderer, &texture, size, Fourcc::Argb8888)
            .unwrap()
            .is_some()
    );
    assert!(
        try_readback(&mut renderer, &texture, size, Fourcc::Argb8888)
            .unwrap()
            .is_some()
    );
}
