use std::io::Cursor;

use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::video::VideoFormat;
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, Property, PropertyFlags};
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};
use smithay::backend::allocator::{format::FormatSet, Fourcc};
use smithay::reexports::gbm::Modifier;
use smithay::utils::{Physical, Size};

pub(super) fn make_video_params(
    video_formats: &[VideoFormat],
    modifiers: &[Modifier],
    size: Size<u32, Physical>,
    refresh: u32,
    fixated: bool,
) -> pod::Object {
    let modifier_property = if modifiers.is_empty() {
        None
    } else {
        let dont_fixate = if modifier_choice_needs_fixation(fixated, modifiers) {
            PropertyFlags::DONT_FIXATE
        } else {
            PropertyFlags::empty()
        };
        let flags = PropertyFlags::MANDATORY | dont_fixate;
        let modifiers_i64 = modifiers
            .iter()
            .map(|m| u64::from(*m) as i64)
            .collect::<Vec<_>>();
        Some(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags,
            value: pod::Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: modifiers_i64[0],
                    alternatives: modifiers_i64,
                },
            ))),
        })
    };

    pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: [
            vec![
                pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
                pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            ],
            video_formats
                .iter()
                .map(|video_format| pod::property!(FormatProperties::VideoFormat, Id, video_format))
                .collect(),
            match modifier_property {
                Some(prop) => vec![prop],
                None => vec![],
            },
            vec![
                pod::property!(
                    FormatProperties::VideoSize,
                    Rectangle,
                    Rectangle {
                        width: size.w,
                        height: size.h,
                    }
                ),
                pod::property!(
                    FormatProperties::VideoFramerate,
                    Fraction,
                    Fraction { num: 0, denom: 1 }
                ),
                pod::property!(
                    FormatProperties::VideoMaxFramerate,
                    Choice,
                    Range,
                    Fraction,
                    Fraction {
                        num: refresh,
                        denom: 1000
                    },
                    Fraction { num: 1, denom: 1 },
                    Fraction {
                        num: refresh,
                        denom: 1000
                    }
                ),
            ],
        ]
        .concat(),
    }
}

pub(super) fn modifier_choice_needs_fixation(fixated: bool, modifiers: &[Modifier]) -> bool {
    !fixated && (modifiers.len() > 1 || modifiers == [Modifier::Invalid])
}

/// this function return an extra Vec<u8> to avoid extra allocation when building Pod
pub(super) fn make_video_params_for_initial_negotiation_with_extra_buffer(
    possible_modifiers: &FormatSet,
    size: Size<u32, Physical>,
    refresh: u32,
    alpha: bool,
) -> Vec<(pod::Object, Vec<u8>)> {
    let f = |alpha| {
        let video_formats = if alpha {
            vec![VideoFormat::BGRA]
        } else {
            vec![VideoFormat::BGRx]
        };

        let fourcc = if alpha {
            Fourcc::Argb8888
        } else {
            Fourcc::Xrgb8888
        };

        let modifiers: Vec<_> = possible_modifiers
            .iter()
            .filter_map(|f| (f.code == fourcc).then_some(f.modifier))
            .collect();

        trace!("offering: {modifiers:?}");

        if modifiers.is_empty() {
            vec![(
                make_video_params(&video_formats, &[], size, refresh, false),
                Vec::new(),
            )]
        } else {
            vec![
                (
                    make_video_params(&video_formats, &modifiers, size, refresh, false),
                    Vec::new(),
                ),
                (
                    make_video_params(&video_formats, &[], size, refresh, false),
                    Vec::new(),
                ),
            ]
        }
    };
    if alpha {
        [f(true), f(false)].concat()
    } else {
        f(false)
    }
}

macro_rules! make_video_params_for_initial_negotiation_macro {
    ($params:ident, $formats:expr, $size:expr, $refresh:expr, $alpha:expr) => {
        let mut $params = $crate::screencasting::pw_utils::formats::make_video_params_for_initial_negotiation_with_extra_buffer(
            $formats, $size, $refresh, $alpha,
        );
        let $params: Vec<_> = $params
            .iter_mut()
            .map(|(obj, buf)| $crate::screencasting::pw_utils::formats::make_pod(buf, (*obj).clone()))
            .collect();
    };
}

pub(super) fn make_pod(buffer: &mut Vec<u8>, object: pod::Object) -> &Pod {
    PodSerializer::serialize(Cursor::new(&mut *buffer), &pod::Value::Object(object)).unwrap();
    Pod::from_bytes(buffer).unwrap()
}

pub(super) use make_video_params_for_initial_negotiation_macro;
