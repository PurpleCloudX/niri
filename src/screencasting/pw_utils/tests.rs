use super::formats::modifier_choice_needs_fixation;
use super::formats::{make_pod, make_video_params_for_initial_negotiation_with_extra_buffer};
use super::*;
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
