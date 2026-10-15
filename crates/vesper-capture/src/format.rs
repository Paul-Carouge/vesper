//! SPA format pods, SPA to DRM fourcc mapping and timestamps.

use std::time::Duration;

use pipewire::spa::param::video::VideoFormat;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{
    ChoiceValue, Object, Property, PropertyFlags, Value, deserialize::PodDeserializer,
};
use pipewire::spa::sys;
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

/// Maps a SPA video format to its DRM fourcc. SPA names bytes in memory
/// order, DRM names a little-endian word, so `BGRx` is `XRGB8888`.
pub fn spa_to_drm(format: VideoFormat) -> Option<u32> {
    Some(match format {
        VideoFormat::BGRx => fourcc(b"XR24"),
        VideoFormat::BGRA => fourcc(b"AR24"),
        VideoFormat::RGBx => fourcc(b"XB24"),
        VideoFormat::RGBA => fourcc(b"AB24"),
        VideoFormat::xRGB => fourcc(b"BX24"),
        VideoFormat::ARGB => fourcc(b"BA24"),
        VideoFormat::xBGR => fourcc(b"RX24"),
        VideoFormat::ABGR => fourcc(b"RA24"),
        // No 10-bit 2:10:10:10 yet: the encoder's VPP path only takes 8-bit RGB.
        _ => return None,
    })
}

/// The video properties of a format pod we care about.
#[derive(Debug, Default, PartialEq)]
pub struct FormatInfo {
    pub formats: Vec<VideoFormat>,
    pub modifiers: Vec<u64>,
    /// The modifier is a choice the client must fixate.
    pub dont_fixate: bool,
    pub size: Option<Rectangle>,
}

fn choice_values<T: Copy + pipewire::spa::pod::CanonicalFixedSizedPod>(
    choice: &ChoiceEnum<T>,
) -> Vec<T> {
    match choice {
        ChoiceEnum::None(v) => vec![*v],
        ChoiceEnum::Enum {
            default,
            alternatives,
        } => std::iter::once(*default)
            .chain(alternatives.iter().copied())
            .collect(),
        ChoiceEnum::Range { default, .. }
        | ChoiceEnum::Step { default, .. }
        | ChoiceEnum::Flags { default, .. } => vec![*default],
    }
}

fn dedup<T: PartialEq>(values: Vec<T>) -> Vec<T> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// Parses a raw-video `Format`/`EnumFormat` pod. Returns `None` for other
/// media types.
pub fn parse_format(bytes: &[u8]) -> Option<FormatInfo> {
    let Ok((_, Value::Object(obj))) = PodDeserializer::deserialize_any_from(bytes) else {
        return None;
    };
    let mut info = FormatInfo::default();
    for prop in obj.properties {
        match (prop.key, prop.value) {
            (sys::SPA_FORMAT_mediaType, Value::Id(Id(t))) if t != sys::SPA_MEDIA_TYPE_video => {
                return None;
            }
            (sys::SPA_FORMAT_mediaSubtype, Value::Id(Id(t))) if t != sys::SPA_MEDIA_SUBTYPE_raw => {
                return None;
            }
            (sys::SPA_FORMAT_VIDEO_format, Value::Id(Id(f))) => {
                info.formats = vec![VideoFormat::from_raw(f)];
            }
            (sys::SPA_FORMAT_VIDEO_format, Value::Choice(ChoiceValue::Id(Choice(_, c)))) => {
                info.formats = dedup(choice_values(&c))
                    .into_iter()
                    .map(|Id(f)| VideoFormat::from_raw(f))
                    .collect();
            }
            (sys::SPA_FORMAT_VIDEO_modifier, Value::Long(m)) => info.modifiers = vec![m as u64],
            (sys::SPA_FORMAT_VIDEO_modifier, Value::Choice(ChoiceValue::Long(Choice(_, c)))) => {
                info.dont_fixate = prop.flags.contains(PropertyFlags::DONT_FIXATE);
                info.modifiers = dedup(choice_values(&c))
                    .into_iter()
                    .map(|m| m as u64)
                    .collect();
            }
            (sys::SPA_FORMAT_VIDEO_size, Value::Rectangle(r)) => info.size = Some(r),
            // Servers often wrap even a fixed size in a choice: take its default.
            (sys::SPA_FORMAT_VIDEO_size, Value::Choice(ChoiceValue::Rectangle(Choice(_, c)))) => {
                info.size = choice_values(&c).first().copied();
            }
            _ => {}
        }
    }
    Some(info)
}

fn serialize(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("pod serialization into a Vec cannot fail")
        .0
        .into_inner()
}

/// Builds a DMA-BUF `EnumFormat` pod. One modifier is a fixated choice;
/// several let the server pick. `modifiers` must not be empty.
pub fn enum_format(format: VideoFormat, modifiers: &[u64]) -> Vec<u8> {
    let range =
        |default, min, max| Value::Choice(ChoiceValue::Rectangle(range_choice(default, min, max)));
    let mut properties = vec![
        Property::new(
            sys::SPA_FORMAT_mediaType,
            Value::Id(Id(sys::SPA_MEDIA_TYPE_video)),
        ),
        Property::new(
            sys::SPA_FORMAT_mediaSubtype,
            Value::Id(Id(sys::SPA_MEDIA_SUBTYPE_raw)),
        ),
        Property::new(sys::SPA_FORMAT_VIDEO_format, Value::Id(Id(format.as_raw()))),
        Property::new(
            sys::SPA_FORMAT_VIDEO_size,
            range(
                Rectangle {
                    width: 1920,
                    height: 1080,
                },
                Rectangle {
                    width: 1,
                    height: 1,
                },
                Rectangle {
                    width: 16384,
                    height: 16384,
                },
            ),
        ),
        Property::new(
            sys::SPA_FORMAT_VIDEO_framerate,
            Value::Choice(ChoiceValue::Fraction(range_choice(
                Fraction { num: 60, denom: 1 },
                Fraction { num: 0, denom: 1 },
                Fraction {
                    num: 1000,
                    denom: 1,
                },
            ))),
        ),
    ];
    match modifiers {
        [] => unreachable!("DMA-BUF formats always carry modifiers"),
        [m] => properties.push(Property {
            key: sys::SPA_FORMAT_VIDEO_modifier,
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(*m as i64),
        }),
        [first, ..] => properties.push(Property {
            key: sys::SPA_FORMAT_VIDEO_modifier,
            flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: *first as i64,
                    alternatives: modifiers.iter().map(|&m| m as i64).collect(),
                },
            ))),
        }),
    }
    serialize(Object {
        type_: sys::SPA_TYPE_OBJECT_Format,
        id: sys::SPA_PARAM_EnumFormat,
        properties,
    })
}

fn range_choice<T: pipewire::spa::pod::CanonicalFixedSizedPod>(
    default: T,
    min: T,
    max: T,
) -> Choice<T> {
    Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range { default, min, max },
    )
}

/// The `Buffers` param: DMA-BUF only.
pub fn buffers_param() -> Vec<u8> {
    serialize(Object {
        type_: sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: sys::SPA_PARAM_Buffers,
        properties: vec![Property::new(
            sys::SPA_PARAM_BUFFERS_dataType,
            Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: 1 << sys::SPA_DATA_DmaBuf,
                    flags: vec![],
                },
            ))),
        )],
    })
}

/// Requests `spa_meta_header`, which carries the presentation timestamp.
pub fn header_meta_param() -> Vec<u8> {
    serialize(Object {
        type_: sys::SPA_TYPE_OBJECT_ParamMeta,
        id: sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(
                sys::SPA_PARAM_META_type,
                Value::Id(Id(sys::SPA_META_Header)),
            ),
            Property::new(
                sys::SPA_PARAM_META_size,
                Value::Int(size_of::<sys::spa_meta_header>() as i32),
            ),
        ],
    })
}

/// `CLOCK_MONOTONIC` now.
pub fn monotonic_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid pointer; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Frame pts: the header pts (`CLOCK_MONOTONIC` ns) when the producer set
/// one, else the time we got the buffer.
pub fn frame_pts(header_pts: Option<i64>) -> Duration {
    match header_pts {
        Some(ns) if ns > 0 => Duration::from_nanos(ns as u64),
        _ => monotonic_now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_formats_to_drm() {
        // DRM_FORMAT_XRGB8888 = 0x34325258.
        assert_eq!(spa_to_drm(VideoFormat::BGRx), Some(0x3432_5258));
        assert_eq!(spa_to_drm(VideoFormat::BGRA), Some(fourcc(b"AR24")));
        assert_eq!(spa_to_drm(VideoFormat::RGBx), Some(fourcc(b"XB24")));
        assert_eq!(spa_to_drm(VideoFormat::RGBA), Some(fourcc(b"AB24")));
        assert_eq!(spa_to_drm(VideoFormat::ABGR_210LE), None);
        assert_eq!(spa_to_drm(VideoFormat::NV12), None);
        assert_eq!(spa_to_drm(VideoFormat::RGB), None);
    }

    #[test]
    fn header_pts_wins_and_invalid_falls_back_to_clock() {
        assert_eq!(frame_pts(Some(1_500)), Duration::from_nanos(1_500));
        let before = monotonic_now();
        let fallback = frame_pts(Some(-1));
        assert!(fallback >= before && fallback <= monotonic_now());
        assert!(frame_pts(None) >= fallback);
    }

    #[test]
    fn enum_format_round_trips() {
        const MOD_LINEAR: u64 = 0;
        let mods = [0x0200_0000_0000_0001, MOD_LINEAR];
        let info = parse_format(&enum_format(VideoFormat::BGRx, &mods)).unwrap();
        assert_eq!(info.formats, vec![VideoFormat::BGRx]);
        assert_eq!(info.modifiers, mods);
        assert!(info.dont_fixate);
        // The size is a range choice: its default must be read, not dropped.
        assert_eq!(info.size.map(|r| (r.width, r.height)), Some((1920, 1080)));

        let fixed = parse_format(&enum_format(VideoFormat::BGRA, &[MOD_LINEAR])).unwrap();
        assert_eq!(fixed.modifiers, vec![MOD_LINEAR]);
        assert!(!fixed.dont_fixate);
    }
}
