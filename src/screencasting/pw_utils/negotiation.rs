use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Context as _;
use pipewire::spa;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, PodPropFlags, Property};
use pipewire::spa::sys::*;
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, SpaTypes};
use pipewire::stream::Stream;
use smithay::backend::allocator::{format::FormatSet, gbm::GbmDevice, Fourcc};
use smithay::backend::drm::DrmDeviceFd;
use smithay::reexports::gbm::Modifier;
use smithay::utils::{Physical, Size};

use super::formats::{
    make_pod, make_video_params, make_video_params_for_initial_negotiation_macro,
    make_video_params_for_initial_negotiation_with_extra_buffer,
};
use super::{
    find_preferred_modifier, negotiated_frame_interval, CastInner, CastState, DmaNegotiationResult,
    ShmLayout, CURSOR_META_SIZE, SHM_BLOCKS,
};
use crate::dbus::mutter_screen_cast::CursorMode;
use crate::utils::CastStreamId;

/// Owns the format callback's dependencies; buffer allocation remains on Cast.
pub(super) fn listener(
    inner: Rc<RefCell<CastInner>>,
    stop_cast: impl Fn() + 'static,
    gbm: GbmDevice<DrmDeviceFd>,
    formats: FormatSet,
    stream_id: CastStreamId,
    cursor_mode: CursorMode,
) -> impl Fn(&Stream, &mut (), u32, Option<&Pod>) {
    move |stream, (), id, pod| {
        let id = ParamType::from_raw(id);
        trace!(%stream_id, ?id, "param_changed");
        let mut inner = inner.borrow_mut();
        let inner = &mut *inner;
        let refresh = inner.refresh;

        if id != ParamType::Format {
            return;
        }

        let _span = debug_span!("param_changed", %stream_id).entered();

        let Some(pod) = pod else { return };

        let (m_type, m_subtype) = match parse_format(pod) {
            Ok(x) => x,
            Err(err) => {
                warn!("error parsing format: {err:?}");
                return;
            }
        };

        if m_type != MediaType::Video || m_subtype != MediaSubtype::Raw {
            return;
        }

        let mut format = VideoInfoRaw::new();
        if let Err(err) = format.parse(pod) {
            warn!("error parsing raw video format: {err:?}");
            stop_cast();
            return;
        }
        if !matches!(format.format(), VideoFormat::BGRA | VideoFormat::BGRx) {
            warn!("consumer selected an unsupported video format");
            stop_cast();
            return;
        }
        debug!("got format = {format:?}");

        let format_size = Size::from((format.size().width, format.size().height));
        let dma_failed = inner.dma_failed;

        let state = &mut inner.state;
        if format_size != state.expected_format_size() {
            if !matches!(&*state, CastState::ResizePending { .. }) {
                warn!("wrong size, but we're not resizing");
                stop_cast();
                return;
            }

            debug!("wrong size, waiting");
            return;
        }

        let format_has_alpha = format.format() == VideoFormat::BGRA;
        let fourcc = if format_has_alpha {
            Fourcc::Argb8888
        } else {
            Fourcc::Xrgb8888
        };

        let max_frame_rate = format.max_framerate();
        // A missing/variable maximum rate is represented by a zero numerator.
        // Retain the existing output limit rather than dividing by zero.
        if let Some(interval) = negotiated_frame_interval(max_frame_rate) {
            inner.min_time_between_frames = interval;
        }

        // We have following cases when param_changed:
        //
        // 1. Modifier exists and its flags contain DONT_FIXATE
        //
        //    Do test allocation, set CastState to ConfirmationPending and send param
        //    again.
        //
        // 2. Modifier exists and it doesn't need fixation
        //
        //    Do test allocation to ensure the modifier work, then set CastState to
        //    Ready. Then set buffer to DMA.
        //
        // 3. Modifier doesn't exist
        //
        //    TODO: set CastState to Ready and set buffer to SHM.

        let object = pod.as_object().unwrap();
        let maybe_prop_modifier =
            object.find_prop(spa::utils::Id(FormatProperties::VideoModifier.0));

        if (dma_failed
            || matches!(
                *state,
                CastState::ConfirmationPending {
                    extra_negotiation_result: None,
                    ..
                }
            ))
            && maybe_prop_modifier.is_some()
        {
            warn!("consumer returned DMA-BUF after SHM-only negotiation");
            stop_cast();
            return;
        }

        match maybe_prop_modifier {
            Some(prop_modifier) if prop_modifier.flags().contains(PodPropFlags::DONT_FIXATE) => {
                debug!("fixating the modifier");

                let pod_modifier = prop_modifier.value();
                let Ok((_, modifiers)) =
                    PodDeserializer::deserialize_from::<Choice<i64>>(pod_modifier.as_bytes())
                else {
                    warn!("wrong modifier property type");
                    stop_cast();
                    return;
                };

                let ChoiceEnum::Enum { alternatives, .. } = modifiers.1 else {
                    warn!("wrong modifier choice type");
                    stop_cast();
                    return;
                };

                let (modifier, plane_count) =
                    match find_preferred_modifier(&gbm, format_size, fourcc, alternatives) {
                        Ok(x) => x,
                        Err(err) => {
                            warn!("couldn't find preferred modifier: {err:?}");
                            if let Err(err) = request_shm_fallback(
                                stream,
                                state,
                                format_size,
                                format_has_alpha,
                                refresh,
                            ) {
                                warn!("error negotiating SHM fallback: {err:?}");
                                stop_cast();
                            }
                            return;
                        }
                    };

                debug!(
                    "allocation successful \
                     (modifier={modifier:?}, plane_count={plane_count}), \
                     moving to confirmation pending"
                );

                *state = CastState::ConfirmationPending {
                    size: format_size,
                    alpha: format_has_alpha,
                    extra_negotiation_result: Some(DmaNegotiationResult {
                        modifier,
                        plane_count: plane_count as i32,
                    }),
                };

                let o =
                    make_video_params(&[format.format()], &[modifier], format_size, refresh, true);
                let mut b = Vec::new();
                let pod = make_pod(&mut b, o);
                let params_1 = vec![pod];

                make_video_params_for_initial_negotiation_macro!(
                    params_2,
                    &formats,
                    format_size,
                    refresh,
                    format_has_alpha
                );

                let params = [params_1, params_2].concat();

                if let Err(err) = stream.update_params(params.clone().as_mut_slice()) {
                    warn!("error updating stream params: {err:?}");
                    stop_cast();
                }
            }
            _ => {
                let o1 = match maybe_prop_modifier {
                    Some(_) => {
                        // Verify that alpha and modifier didn't change.
                        let plane_count = match &*state {
                            CastState::ConfirmationPending {
                                size,
                                alpha,
                                extra_negotiation_result,
                            }
                            | CastState::Ready {
                                size,
                                alpha,
                                extra_negotiation_result,
                                ..
                            } if *alpha == format_has_alpha
                                && matches!(
                                    extra_negotiation_result,
                                    Some(x) if x.modifier == Modifier::from(format.modifier())
                                ) =>
                            {
                                let size = *size;
                                let alpha = *alpha;
                                let extra_negotiation_result = *extra_negotiation_result;

                                let (damage_tracker, cursor_damage_tracker) =
                                    if let CastState::Ready {
                                        damage_tracker,
                                        cursor_damage_tracker,
                                        ..
                                    } = &mut *state
                                    {
                                        (damage_tracker.take(), cursor_damage_tracker.take())
                                    } else {
                                        (None, None)
                                    };

                                debug!("moving to ready state");

                                *state = CastState::Ready {
                                    size,
                                    alpha,
                                    extra_negotiation_result,
                                    damage_tracker,
                                    cursor_damage_tracker,
                                    last_cursor_location: None,
                                };

                                extra_negotiation_result.unwrap().plane_count
                            }
                            _ => {
                                // We're negotiating a single modifier, or alpha or modifier changed,
                                // so we need to do a test allocation.
                                let (modifier, plane_count) = match find_preferred_modifier(
                                    &gbm,
                                    format_size,
                                    fourcc,
                                    vec![format.modifier() as i64],
                                ) {
                                    Ok(x) => x,
                                    Err(err) => {
                                        warn!("test allocation failed: {err:?}");
                                        if let Err(err) = request_shm_fallback(
                                            stream,
                                            state,
                                            format_size,
                                            format_has_alpha,
                                            refresh,
                                        ) {
                                            warn!("error negotiating SHM fallback: {err:?}");
                                            stop_cast();
                                        }
                                        return;
                                    }
                                };

                                debug!(
                                    "allocation successful \
                                     (modifier={modifier:?}, plane_count={plane_count}), \
                                     moving to ready"
                                );

                                *state = CastState::Ready {
                                    size: format_size,
                                    alpha: format_has_alpha,
                                    extra_negotiation_result: Some(DmaNegotiationResult {
                                        modifier,
                                        plane_count: plane_count as i32,
                                    }),
                                    damage_tracker: None,
                                    cursor_damage_tracker: None,
                                    last_cursor_location: None,
                                };

                                plane_count as i32
                            }
                        };
                        // const BPP: u32 = 4;
                        // let stride = format.size().width * BPP;
                        // let size = stride * format.size().height;

                        pod::object!(
                            SpaTypes::ObjectParamBuffers,
                            ParamType::Buffers,
                            Property::new(
                                SPA_PARAM_BUFFERS_buffers,
                                pod::Value::Choice(ChoiceValue::Int(Choice(
                                    ChoiceFlags::empty(),
                                    ChoiceEnum::Range {
                                        default: 8,
                                        min: 2,
                                        max: 16
                                    }
                                ))),
                            ),
                            Property::new(SPA_PARAM_BUFFERS_blocks, pod::Value::Int(plane_count)),
                            Property::new(
                                SPA_PARAM_BUFFERS_dataType,
                                pod::Value::Choice(ChoiceValue::Int(Choice(
                                    ChoiceFlags::empty(),
                                    ChoiceEnum::Flags {
                                        default: 1 << DataType::DmaBuf.as_raw(),
                                        flags: vec![1 << DataType::DmaBuf.as_raw()],
                                    },
                                ))),
                            ),
                        )
                    }
                    None => {
                        *state = CastState::Ready {
                            size: format_size,
                            alpha: format_has_alpha,
                            extra_negotiation_result: None,
                            damage_tracker: None,
                            cursor_damage_tracker: None,
                            last_cursor_location: None,
                        };
                        pod::object!(
                            SpaTypes::ObjectParamBuffers,
                            ParamType::Buffers,
                            Property::new(
                                SPA_PARAM_BUFFERS_buffers,
                                pod::Value::Choice(ChoiceValue::Int(Choice(
                                    ChoiceFlags::empty(),
                                    ChoiceEnum::Range {
                                        default: 16,
                                        min: 2,
                                        max: 16
                                    }
                                ))),
                            ),
                            Property::new(
                                SPA_PARAM_BUFFERS_blocks,
                                pod::Value::Int(SHM_BLOCKS as i32),
                            ),
                            Property::new(
                                SPA_PARAM_BUFFERS_dataType,
                                pod::Value::Choice(ChoiceValue::Int(Choice(
                                    ChoiceFlags::empty(),
                                    ChoiceEnum::Flags {
                                        default: 1 << DataType::MemFd.as_raw(),
                                        flags: vec![1 << DataType::MemFd.as_raw()],
                                    },
                                ))),
                            ),
                        )
                    }
                };
                let o2 = pod::object!(
                    SpaTypes::ObjectParamMeta,
                    ParamType::Meta,
                    Property::new(
                        SPA_PARAM_META_type,
                        pod::Value::Id(spa::utils::Id(SPA_META_Header))
                    ),
                    Property::new(
                        SPA_PARAM_META_size,
                        pod::Value::Int(size_of::<spa_meta_header>() as i32)
                    ),
                );

                let mut b1 = vec![];
                let mut b2 = vec![];

                let mut params = vec![make_pod(&mut b1, o1), make_pod(&mut b2, o2)];

                let mut b_cursor = vec![];
                if cursor_mode == CursorMode::Metadata {
                    let o_cursor = pod::object!(
                        SpaTypes::ObjectParamMeta,
                        ParamType::Meta,
                        Property::new(
                            SPA_PARAM_META_type,
                            pod::Value::Id(spa::utils::Id(SPA_META_Cursor))
                        ),
                        Property::new(
                            SPA_PARAM_META_size,
                            pod::Value::Int(CURSOR_META_SIZE as i32)
                        ),
                    );
                    params.push(make_pod(&mut b_cursor, o_cursor));
                }

                if let Err(err) = stream.update_params(&mut params) {
                    warn!("error updating stream params: {err:?}");
                    stop_cast();
                }
            }
        };
    }
}

pub(super) fn request_shm_fallback(
    stream: &Stream,
    state: &mut CastState,
    size: Size<u32, Physical>,
    alpha: bool,
    refresh: u32,
) -> anyhow::Result<()> {
    ShmLayout::new(size)?;
    *state = CastState::ConfirmationPending {
        size,
        alpha,
        extra_negotiation_result: None,
    };
    // An empty format set advertises only modifier-less MemFd alternatives.
    let mut objects = make_video_params_for_initial_negotiation_with_extra_buffer(
        &FormatSet::default(),
        size,
        refresh,
        alpha,
    );
    let mut params: Vec<_> = objects
        .iter_mut()
        .map(|(object, bytes)| make_pod(bytes, object.clone()))
        .collect();
    debug!(?size, alpha, "retrying negotiation with SHM only");
    stream
        .update_params(&mut params)
        .context("error publishing SHM-only formats")
}
