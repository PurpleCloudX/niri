use super::*;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::ExportMem;

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
    assert_eq!(staging.size.as_ref().unwrap().2, other.context_id());
}
