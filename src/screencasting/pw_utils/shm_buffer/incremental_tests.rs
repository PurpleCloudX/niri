use super::*;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
use smithay::backend::renderer::{element::Kind, ExportMem};
use std::os::unix::fs::FileExt;

#[test]
#[ignore = "manual SHM render/readback/copy benchmark, excluding PipeWire delivery"]
fn egl_incremental_shm_benchmark() {
    use std::time::{Duration, Instant};
    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    };
    for round in 0..3 {
        for scene in ["static", "small", "full"] {
            let order = if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for incremental in order {
                let mut staging = StagingTexture::default();
                let buffers = (0..3)
                    .map(|_| allocate_shmbuf(Size::from((1920, 1080))).unwrap())
                    .collect::<Vec<_>>();
                let mut background = SolidColorBuffer::new((1920.0, 1080.0), [0.0, 0.0, 0.5, 1.0]);
                let mut foreground = SolidColorBuffer::new((64.0, 64.0), [0.5, 0.0, 0.0, 1.0]);
                let mut elapsed = Duration::ZERO;
                for frame in 0..100 {
                    let color = if frame % 2 == 0 {
                        [0.5, 0.0, 0.0, 1.0]
                    } else {
                        [0.0, 0.5, 0.0, 1.0]
                    };
                    if scene == "small" {
                        foreground.set_color(color);
                    }
                    if scene == "full" {
                        background.set_color(color);
                    }
                    let elements = [
                        SolidColorRenderElement::from_buffer(
                            &foreground,
                            (31.0, 27.0),
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
                    let buffer = &buffers[frame % 3];
                    if !incremental {
                        *buffer.content.borrow_mut() = None;
                    }
                    let start = Instant::now();
                    let (pending, fence) = render_to_shmbuf(
                        &mut renderer,
                        &mut staging,
                        buffer,
                        Size::from((1920, 1080)),
                        Scale::from(1.0),
                        Transform::Normal,
                        Fourcc::Argb8888,
                        &elements,
                    )
                    .unwrap();
                    pending.complete(&mut renderer).unwrap();
                    drop(fence);
                    if frame >= 20 {
                        elapsed += start.elapsed();
                    }
                }
                eprintln!(
                    "shm round={round} scene={scene} incremental={incremental} ms/frame={:.3}",
                    elapsed.as_secs_f64() * 1000.0 / 80.0
                );
            }
        }
    }
}

#[test]
fn egl_partial_updates_match_full_frames_across_buffer_rotation() {
    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    };
    let size = Size::from((96, 64));
    let buffers = (0..3)
        .map(|_| allocate_shmbuf(Size::from((96, 64))).unwrap())
        .collect::<Vec<_>>();
    let mut staging = StagingTexture::default();
    let mut reference = StagingTexture::default();
    let background = SolidColorBuffer::new((96.0, 64.0), [0.1, 0.2, 0.3, 0.6]);
    let mut foreground = SolidColorBuffer::new((7.0, 5.0), [0.0, 0.8, 0.0, 0.8]);
    let mut partial = 0;
    for frame in 0..40 {
        let buffer = &buffers[frame % 3];
        if frame == 9 {
            clear_shmbuf(buffer).unwrap();
        }
        let transform = if (24..32).contains(&frame) {
            [
                Transform::Normal,
                Transform::_90,
                Transform::_180,
                Transform::_270,
                Transform::Flipped,
                Transform::Flipped90,
                Transform::Flipped180,
                Transform::Flipped270,
            ][frame - 24]
        } else if (12..15).contains(&frame) {
            Transform::Flipped180
        } else {
            Transform::Normal
        };
        let scale = if frame >= 18 {
            Scale::from(1.25)
        } else {
            Scale::from(1.0)
        };
        foreground.set_color(if frame % 2 == 0 {
            [0.8, 0.0, 0.0, 0.8]
        } else {
            [0.0, 0.8, 0.0, 0.8]
        });
        let elements = [
            SolidColorRenderElement::from_buffer(
                &foreground,
                (11.0 + (frame % 4) as f64, 17.0),
                1.0,
                Kind::Unspecified,
            ),
            SolidColorRenderElement::from_buffer(&background, (0.0, 0.0), 1.0, Kind::Unspecified),
        ];
        let busy = if frame == 6 {
            Some(
                reference
                    .render_and_readback(
                        &mut renderer,
                        size,
                        scale,
                        transform,
                        Fourcc::Argb8888,
                        &elements,
                    )
                    .unwrap(),
            )
        } else {
            None
        };
        let (pending, fence) = render_to_shmbuf(
            &mut renderer,
            &mut staging,
            buffer,
            size,
            scale,
            transform,
            Fourcc::Argb8888,
            &elements,
        )
        .unwrap();
        if frame == 7 {
            // The destination must retain its old stamp if GPU readback is cancelled.
            drop(pending);
            drop(fence);
            continue;
        }
        if pending.region.is_some_and(|rect| rect.size != size) {
            partial += 1;
        }
        if frame == 6 || frame == 9 || (12..15).contains(&frame) || (25..32).contains(&frame) {
            assert_eq!(pending.region, Some(Rectangle::from_size(size)));
        }
        pending.complete(&mut renderer).unwrap();
        drop(busy);
        drop(fence);
        let mapping = reference
            .render_and_download(
                &mut renderer,
                size,
                scale,
                transform,
                Fourcc::Argb8888,
                &elements,
            )
            .unwrap();
        let expected = renderer.map_texture(&mapping).unwrap();
        let mut actual = vec![0; buffer.layout.size_usize()];
        std::fs::File::from(buffer.fd.try_clone().unwrap())
            .read_exact_at(&mut actual, 0)
            .unwrap();
        assert!(
            actual == expected,
            "frame {frame}, transform {transform:?}, first differing byte {:?}",
            actual.iter().zip(expected).position(|(a, b)| a != b)
        );
    }
    assert!(
        partial >= 6,
        "test must exercise partial readback, got {partial}"
    );
}

#[test]
fn egl_unchanged_destination_skips_readback_but_new_destination_is_initialized() {
    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).unwrap();
        GlesRenderer::new(EGLContext::new(&display).unwrap()).unwrap()
    };
    let mut staging = StagingTexture::default();
    let buffer = allocate_shmbuf(Size::from((16, 8))).unwrap();
    for frame in 0..3 {
        let (pending, fence) = render_to_shmbuf(
            &mut renderer,
            &mut staging,
            &buffer,
            Size::from((16, 8)),
            Scale::from(1.0),
            Transform::Normal,
            Fourcc::Argb8888,
            &[] as &[SolidColorRenderElement],
        )
        .unwrap();
        assert_eq!(pending.region.is_none(), frame != 0);
        pending.complete(&mut renderer).unwrap();
        drop(fence);
    }
    let new_buffer = allocate_shmbuf(Size::from((16, 8))).unwrap();
    let (pending, _) = render_to_shmbuf(
        &mut renderer,
        &mut staging,
        &new_buffer,
        Size::from((16, 8)),
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Argb8888,
        &[] as &[SolidColorRenderElement],
    )
    .unwrap();
    assert_eq!(pending.region, Some(Rectangle::from_size((16, 8).into())));
}
