use super::*;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::ExportMem;

#[test]
#[ignore = "manual partial readback comparison"]
fn egl_partial_readback_benchmark() {
    use smithay::backend::renderer::gles::ffi;
    use std::{ptr, time::Instant};
    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    };
    let texture =
        create_texture(&mut renderer, Size::from((1920, 1080)), Fourcc::Argb8888).unwrap();
    renderer
        .with_context(|gl| unsafe {
            let mut fbo = 0;
            let mut pbo = 0;
            gl.GenFramebuffers(1, &mut fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                texture.tex_id(),
                0,
            );
            assert_eq!(
                gl.CheckFramebufferStatus(ffi::FRAMEBUFFER),
                ffi::FRAMEBUFFER_COMPLETE
            );
            gl.GenBuffers(1, &mut pbo);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, pbo);
            gl.BufferData(
                ffi::PIXEL_PACK_BUFFER,
                1920 * 1080 * 4,
                ptr::null(),
                ffi::STREAM_READ,
            );
            let mut destination = vec![0u8; 1920 * 1080 * 4];
            for round in 0..3 {
                let mut cases = vec![(1920, 1080), (64, 64), (960, 540), (1920, 540)];
                if round % 2 == 1 {
                    cases.reverse();
                }
                for (w, h) in cases {
                    let mut elapsed = std::time::Duration::ZERO;
                    for frame in 0..120 {
                        gl.ClearColor(1.0, 0.0, 0.0, 1.0);
                        gl.Clear(ffi::COLOR_BUFFER_BIT);
                        let start = Instant::now();
                        gl.ReadPixels(
                            0,
                            0,
                            w,
                            h,
                            ffi::BGRA_EXT,
                            ffi::UNSIGNED_BYTE,
                            ptr::null_mut(),
                        );
                        let mapped = gl.MapBufferRange(
                            ffi::PIXEL_PACK_BUFFER,
                            0,
                            (w * h * 4) as isize,
                            ffi::MAP_READ_BIT,
                        );
                        assert!(!mapped.is_null());
                        for row in 0..h as usize {
                            ptr::copy_nonoverlapping(
                                mapped.cast::<u8>().add(row * w as usize * 4),
                                destination.as_mut_ptr().add(row * 1920 * 4),
                                w as usize * 4,
                            );
                        }
                        assert_eq!(gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER), ffi::TRUE);
                        if frame >= 20 {
                            elapsed += start.elapsed();
                        }
                    }
                    assert_eq!(&destination[..4], &[0, 0, 255, 255]);
                    assert_eq!(gl.GetError(), ffi::NO_ERROR);
                    eprintln!(
                        "partial round={round} size={w}x{h} ms/frame={:.3}",
                        elapsed.as_secs_f64() * 10.0
                    );
                }
            }
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            gl.DeleteBuffers(1, &pbo);
            gl.DeleteFramebuffers(1, &fbo);
        })
        .unwrap();
}

#[test]
#[ignore = "manual PBO allocation comparison; not an end-to-end capture benchmark"]
fn egl_pbo_reuse_benchmark() {
    use smithay::backend::renderer::gles::ffi;
    use std::{ptr, time::Instant};

    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    };
    let texture =
        create_texture(&mut renderer, Size::from((1920, 1080)), Fourcc::Argb8888).unwrap();
    renderer
        .with_context(|gl| unsafe {
            let mut old_fbo = 0;
            let mut old_pack = 0;
            gl.GetIntegerv(ffi::FRAMEBUFFER_BINDING, &mut old_fbo);
            gl.GetIntegerv(ffi::PIXEL_PACK_BUFFER_BINDING, &mut old_pack);
            let mut fbo = 0;
            gl.GenFramebuffers(1, &mut fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                texture.tex_id(),
                0,
            );
            assert_eq!(
                gl.CheckFramebufferStatus(ffi::FRAMEBUFFER),
                ffi::FRAMEBUFFER_COMPLETE
            );
            gl.ClearColor(1.0, 0.0, 0.0, 1.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);
            let bytes = 1920 * 1080 * 4;
            let mut destination = vec![0u8; bytes];
            for round in 0..3 {
                for reuse in [false, true] {
                    let mut pbo = 0;
                    let mut elapsed = std::time::Duration::ZERO;
                    for frame in 0..120 {
                        let start = Instant::now();
                        if !reuse || frame == 0 {
                            gl.GenBuffers(1, &mut pbo);
                            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, pbo);
                            gl.BufferData(
                                ffi::PIXEL_PACK_BUFFER,
                                bytes as isize,
                                ptr::null(),
                                ffi::STREAM_READ,
                            );
                        } else {
                            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, pbo);
                        }
                        gl.ReadPixels(
                            0,
                            0,
                            1920,
                            1080,
                            ffi::RGBA,
                            ffi::UNSIGNED_BYTE,
                            ptr::null_mut(),
                        );
                        let mapped = gl.MapBufferRange(
                            ffi::PIXEL_PACK_BUFFER,
                            0,
                            bytes as isize,
                            ffi::MAP_READ_BIT,
                        );
                        assert!(!mapped.is_null());
                        ptr::copy_nonoverlapping(
                            mapped.cast::<u8>(),
                            destination.as_mut_ptr(),
                            bytes,
                        );
                        assert_eq!(gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER), ffi::TRUE);
                        gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
                        if !reuse {
                            gl.DeleteBuffers(1, &pbo);
                        }
                        if frame >= 20 {
                            elapsed += start.elapsed();
                        }
                    }
                    if reuse {
                        gl.DeleteBuffers(1, &pbo);
                    }
                    assert!(destination.chunks_exact(4).all(|p| p == [255, 0, 0, 255]));
                    assert_eq!(gl.GetError(), ffi::NO_ERROR);
                    eprintln!(
                        "PBO round={round} reuse={reuse} ms/frame={:.3}",
                        elapsed.as_secs_f64() * 10.0
                    );
                }
            }
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, old_pack as u32);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, old_fbo as u32);
            gl.DeleteFramebuffers(1, &fbo);
        })
        .unwrap();
}

#[test]
#[ignore = "manual EGL render/readback microbenchmark, not an end-to-end latency test"]
fn egl_staging_benchmark() {
    fn measure(renderer: &mut GlesRenderer, cached: bool) -> std::time::Duration {
        let size = Size::from((1920, 1080));
        let mut staging = StagingTexture::default();
        let background = SolidColorBuffer::new((1920.0, 1080.0), [0.0, 0.0, 1.0, 1.0]);
        let mut foreground = SolidColorBuffer::new((64.0, 64.0), [1.0, 0.0, 0.0, 1.0]);
        let mut destination = vec![0; 1920 * 1080 * 4];
        let mut total = std::time::Duration::ZERO;
        for frame in 0..70 {
            foreground.set_color(if frame % 2 == 0 {
                [1.0, 0.0, 0.0, 1.0]
            } else {
                [0.0, 1.0, 0.0, 1.0]
            });
            let elements = [
                SolidColorRenderElement::from_buffer(
                    &foreground,
                    (0.0, 0.0),
                    1.0,
                    Kind::Unspecified,
                ),
                SolidColorRenderElement::from_buffer(
                    &background,
                    (0.0, 0.0),
                    1.0,
                    Kind::Unspecified,
                ),
            ];
            let start = std::time::Instant::now();
            let mapping = if cached {
                staging
                    .render_and_download(
                        renderer,
                        size,
                        Scale::from(1.0),
                        Transform::Normal,
                        Fourcc::Argb8888,
                        &elements,
                    )
                    .unwrap()
            } else {
                crate::render_helpers::render_and_download(
                    renderer,
                    size,
                    Scale::from(1.0),
                    Transform::Normal,
                    Fourcc::Argb8888,
                    elements.iter().rev(),
                )
                .unwrap()
            };
            destination.copy_from_slice(renderer.map_texture(&mapping).unwrap());
            std::hint::black_box(&destination);
            if frame >= 10 {
                total += start.elapsed();
            }
        }
        total
    }
    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    };
    for round in 0..3 {
        let (baseline, cached) = if round % 2 == 0 {
            (measure(&mut renderer, false), measure(&mut renderer, true))
        } else {
            let cached = measure(&mut renderer, true);
            (measure(&mut renderer, false), cached)
        };
        eprintln!(
            "round={round} full_ms_per_frame={:.3} cached_ms_per_frame={:.3}",
            baseline.as_secs_f64() * 1000.0 / 60.0,
            cached.as_secs_f64() * 1000.0 / 60.0
        );
    }
}

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
    assert_eq!(staging.size.as_ref().unwrap().renderer, other.context_id());
}
