use super::formats::modifier_choice_needs_fixation;
use super::formats::{make_pod, make_video_params_for_initial_negotiation_with_extra_buffer};
use super::*;

#[test]
fn removing_a_ready_fence_source_closes_fd_and_cancels_callback() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut event_loop = calloop::EventLoop::<usize>::try_new().unwrap();
    let (reader, mut writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    let token = event_loop.handle().insert_source(
        Generic::new(reader, Interest::READ, Mode::OneShot),
        |_, _, calls| {
            *calls += 1;
            Ok(PostAction::Remove)
        },
    ).unwrap();
    writer.write_all(&[1]).unwrap();
    event_loop.handle().remove(token);
    let mut calls = 0;
    event_loop.dispatch(Duration::ZERO, &mut calls).unwrap();
    assert_eq!(calls, 0);
    // Closing a socket with unread data can yield reset rather than EOF.
    let result = writer.read(&mut [0]);
    assert!(matches!(result, Ok(0)) || result.is_err_and(|err| err.kind() == std::io::ErrorKind::ConnectionReset));
}

#[test]
fn fence_completing_during_failed_export_is_deliverable() {
    use smithay::backend::renderer::sync::{Fence, Interrupted};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Default)]
    struct CompletingFence(AtomicBool);
    impl Fence for CompletingFence {
        fn is_signaled(&self) -> bool { self.0.load(Ordering::SeqCst) }
        fn wait(&self) -> Result<(), Interrupted> { panic!("must not block") }
        fn is_exportable(&self) -> bool { true }
        fn export(&self) -> Option<std::os::fd::OwnedFd> {
            self.0.store(true, Ordering::SeqCst);
            None
        }
    }
    assert!(export_pending_fence(&SyncPoint::from(CompletingFence::default())).unwrap().is_none());
}

#[test]
fn failed_fence_export_does_not_imply_gpu_completion() {
    use smithay::backend::renderer::sync::{Fence, Interrupted};

    #[derive(Debug)]
    struct UnexportableFence(bool);
    impl Fence for UnexportableFence {
        fn is_signaled(&self) -> bool { self.0 }
        fn wait(&self) -> Result<(), Interrupted> { panic!("must not block the compositor") }
        fn is_exportable(&self) -> bool { false }
        fn export(&self) -> Option<std::os::fd::OwnedFd> { None }
    }

    assert!(export_pending_fence(&SyncPoint::from(UnexportableFence(false))).is_err());
    assert!(export_pending_fence(&SyncPoint::from(UnexportableFence(true))).unwrap().is_none());
    assert!(export_pending_fence(&SyncPoint::signaled()).unwrap().is_none());
}

#[test]
fn invalidating_an_undelivered_frame_restores_static_scene_damage() {
    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
    use smithay::backend::renderer::element::Kind;

    let buffer = SolidColorBuffer::new((16.0, 8.0), [1.0, 0.0, 0.0, 1.0]);
    let elements = [SolidColorRenderElement::from_buffer(
        &buffer, (0.0, 0.0), 1.0, Kind::Unspecified,
    )];
    let mut state = CastState::Ready {
        size: Size::from((16, 8)),
        alpha: false,
        extra_negotiation_result: None,
        damage_tracker: None,
        cursor_damage_tracker: None,
        last_cursor_location: None,
    };
    let damaged = |state: &mut CastState| {
        let CastState::Ready { damage_tracker, .. } = state else { unreachable!() };
        damage_tracker
            .get_or_insert_with(|| OutputDamageTracker::new((16, 8), 1.0, Transform::Normal))
            .damage_output(1, &elements)
            .unwrap().0.is_some()
    };
    assert!(damaged(&mut state));
    assert!(!damaged(&mut state));
    state.invalidate_damage();
    assert!(damaged(&mut state));
    assert!(!damaged(&mut state));
}

#[test]
fn resuming_clears_damage_history_without_changing_negotiated_layout() {
    let size = Size::from((16, 8));
    let mut state = CastState::Ready {
        size,
        alpha: false,
        extra_negotiation_result: Some(DmaNegotiationResult {
            modifier: Modifier::Linear,
            plane_count: 1,
        }),
        damage_tracker: Some(OutputDamageTracker::new((16, 8), 1.0, Transform::Normal)),
        cursor_damage_tracker: Some(OutputDamageTracker::new((16, 8), 1.0, Transform::Normal)),
        last_cursor_location: Some(Point::from((4, 4))),
    };
    state.invalidate_damage();
    let CastState::Ready {
        size: actual_size,
        alpha,
        extra_negotiation_result,
        damage_tracker,
        cursor_damage_tracker,
        last_cursor_location,
    } = state
    else {
        panic!("negotiation state changed")
    };
    assert_eq!(actual_size, size);
    assert!(!alpha);
    assert_eq!(extra_negotiation_result.unwrap().modifier, Modifier::Linear);
    assert!(damage_tracker.is_none());
    assert!(cursor_damage_tracker.is_none());
    assert!(last_cursor_location.is_none());
}
use pipewire::spa::{
    self,
    param::format::FormatProperties,
    param::video::{VideoFormat, VideoInfoRaw},
};

#[test]
fn unspecified_frame_rates_preserve_the_existing_limit() {
    assert_eq!(
        negotiated_frame_interval(Fraction { num: 0, denom: 1 }),
        None
    );
    assert_eq!(
        negotiated_frame_interval(Fraction { num: 60, denom: 0 }),
        None
    );
    assert_eq!(
        negotiated_frame_interval(Fraction { num: 60, denom: 1 }),
        Some(Duration::from_micros(16_666))
    );
    assert_eq!(
        negotiated_frame_interval(Fraction {
            num: 60_000,
            denom: 1001
        }),
        Some(Duration::from_micros(16_683))
    );
}

#[test]
fn shm_only_offer_excludes_modifiers_and_preserves_alpha_alternatives() {
    for alpha in [false, true] {
        let mut objects = make_video_params_for_initial_negotiation_with_extra_buffer(
            &FormatSet::default(),
            Size::from((1920, 1080)),
            60_000,
            alpha,
        );
        assert_eq!(objects.len(), if alpha { 2 } else { 1 });
        for (index, (object, bytes)) in objects.iter_mut().enumerate() {
            let pod = make_pod(bytes, object.clone());
            assert!(pod
                .as_object()
                .unwrap()
                .find_prop(spa::utils::Id(FormatProperties::VideoModifier.0))
                .is_none());
            let mut format = VideoInfoRaw::new();
            format.parse(pod).unwrap();
            assert_eq!(
                format.format(),
                if alpha && index == 0 {
                    VideoFormat::BGRA
                } else {
                    VideoFormat::BGRx
                }
            );
        }
    }
}

#[test]
fn rendered_dma_buffer_restores_every_plane_without_changing_layout() {
    let mut chunks = [
        spa_chunk {
            offset: 128,
            size: 0,
            stride: 1024,
            flags: SPA_CHUNK_FLAG_CORRUPTED as i32,
        },
        spa_chunk {
            offset: 4096,
            size: 0,
            stride: 256,
            flags: SPA_CHUNK_FLAG_CORRUPTED as i32,
        },
    ];
    let mut data: [spa_data; 2] = unsafe { mem::zeroed() };
    for (data, chunk) in data.iter_mut().zip(chunks.iter_mut()) {
        data.chunk = chunk;
        data.maxsize = 8192;
    }
    let mut spa: spa_buffer = unsafe { mem::zeroed() };
    spa.n_datas = 2;
    spa.datas = data.as_mut_ptr();
    let mut buffer: pw_buffer = unsafe { mem::zeroed() };
    buffer.buffer = &mut spa;
    let mut sequence = 7;
    unsafe {
        mark_buffer_after_render(
            NonNull::from(&mut buffer),
            &mut sequence,
            SharingBuf::DMA(()),
        )
    };
    assert_eq!(sequence, 8);
    for chunk in &chunks {
        assert_eq!(chunk.size, 8192 - chunk.offset);
        assert_eq!(chunk.flags, SPA_CHUNK_FLAG_NONE as i32);
    }
    assert_eq!((chunks[0].stride, chunks[0].offset), (1024, 128));
    assert_eq!((chunks[1].stride, chunks[1].offset), (256, 4096));
}

#[test]
fn modifier_choice_is_fixated_when_negotiation_requires_it() {
    assert!(!modifier_choice_needs_fixation(false, &[]));
    assert!(!modifier_choice_needs_fixation(false, &[Modifier::Linear]));
    assert!(modifier_choice_needs_fixation(false, &[Modifier::Invalid]));
    assert!(modifier_choice_needs_fixation(
        false,
        &[Modifier::Linear, Modifier::Invalid]
    ));

    assert!(!modifier_choice_needs_fixation(true, &[Modifier::Invalid]));
    assert!(!modifier_choice_needs_fixation(
        true,
        &[Modifier::Linear, Modifier::Invalid]
    ));
}
