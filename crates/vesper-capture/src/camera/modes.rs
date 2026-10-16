//! Camera format negotiation: what a camera node offers (its `EnumFormat`
//! params), which mode to ask for, and what the server settled on. Pure
//! functions over SPA pods, so they are unit tested without PipeWire.

use pipewire::spa::param::video::VideoFormat;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::{ChoiceValue, Object, Property, Value};
use pipewire::spa::sys;
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};

use crate::format::{choice_values, dedup, serialize};

/// How a camera sends its pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// Motion JPEG: decoded on the GPU (VAAPI).
    Mjpeg,
    /// Packed 4:2:2, `Y0 U Y1 V`.
    Yuy2,
    /// 4:2:0, a Y plane then an interleaved UV plane.
    Nv12,
}

/// One size and frame rate a camera can send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub encoding: Encoding,
    pub width: u32,
    pub height: u32,
    /// Frames per second as `num / denom`.
    pub fps: (u32, u32),
}

impl Mode {
    fn millifps(&self) -> u64 {
        let (num, denom) = self.fps;
        u64::from(num) * 1000 / u64::from(denom.max(1))
    }
}

/// YUV to RGB matrix of a raw camera picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Quantization range of a raw camera picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// Y 16-235, UV 16-240.
    Limited,
    /// 0-255 (JFIF, so every MJPEG picture).
    Full,
}

/// The parsed video properties of one `EnumFormat` or `Format` pod.
#[derive(Debug, Default, PartialEq)]
struct Parsed {
    encodings: Vec<Encoding>,
    sizes: Vec<Rectangle>,
    rates: Vec<Fraction>,
    matrix: Option<u32>,
    range: Option<u32>,
}

fn encoding(format: VideoFormat) -> Option<Encoding> {
    match format {
        VideoFormat::YUY2 => Some(Encoding::Yuy2),
        VideoFormat::NV12 => Some(Encoding::Nv12),
        _ => None,
    }
}

/// A fixed value the server wrapped in a `Choice::None`, unwrapped.
fn unwrap_none(value: Value) -> Value {
    match value {
        Value::Choice(ChoiceValue::Id(Choice(_, ChoiceEnum::None(v)))) => Value::Id(v),
        Value::Choice(ChoiceValue::Rectangle(Choice(_, ChoiceEnum::None(v)))) => {
            Value::Rectangle(v)
        }
        Value::Choice(ChoiceValue::Fraction(Choice(_, ChoiceEnum::None(v)))) => Value::Fraction(v),
        v => v,
    }
}

/// Parses a video `EnumFormat`/`Format` pod; `None` for anything else.
fn parse(bytes: &[u8]) -> Option<Parsed> {
    let Ok((_, Value::Object(obj))) = PodDeserializer::deserialize_any_from(bytes) else {
        return None;
    };
    let mut p = Parsed::default();
    let mut mjpeg = false;
    let mut formats = Vec::new();
    for prop in obj.properties {
        match (prop.key, unwrap_none(prop.value)) {
            (sys::SPA_FORMAT_mediaType, Value::Id(Id(t))) if t != sys::SPA_MEDIA_TYPE_video => {
                return None;
            }
            (sys::SPA_FORMAT_mediaSubtype, Value::Id(Id(t))) => match t {
                sys::SPA_MEDIA_SUBTYPE_mjpg => mjpeg = true,
                sys::SPA_MEDIA_SUBTYPE_raw => {}
                _ => return None,
            },
            (sys::SPA_FORMAT_VIDEO_format, Value::Id(Id(f))) => formats = vec![f],
            (sys::SPA_FORMAT_VIDEO_format, Value::Choice(ChoiceValue::Id(Choice(_, c)))) => {
                formats = choice_values(&c).into_iter().map(|Id(f)| f).collect();
            }
            (sys::SPA_FORMAT_VIDEO_size, Value::Rectangle(r)) => p.sizes = vec![r],
            (sys::SPA_FORMAT_VIDEO_size, Value::Choice(ChoiceValue::Rectangle(Choice(_, c)))) => {
                p.sizes = dedup(choice_values(&c));
            }
            (sys::SPA_FORMAT_VIDEO_framerate, Value::Fraction(f)) => p.rates = vec![f],
            // shortcut: a range offers only its default, as cameras list discrete rates;
            // read min/max if a camera with ranged rates picks badly.
            (
                sys::SPA_FORMAT_VIDEO_framerate,
                Value::Choice(ChoiceValue::Fraction(Choice(_, c))),
            ) => p.rates = dedup(choice_values(&c)),
            (sys::SPA_FORMAT_VIDEO_colorMatrix, Value::Id(Id(m))) => p.matrix = Some(m),
            (sys::SPA_FORMAT_VIDEO_colorRange, Value::Id(Id(r))) => p.range = Some(r),
            _ => {}
        }
    }
    p.encodings = if mjpeg {
        vec![Encoding::Mjpeg]
    } else {
        dedup(
            formats
                .into_iter()
                .filter_map(|f| encoding(VideoFormat::from_raw(f)))
                .collect(),
        )
    };
    Some(p)
}

/// Every mode one `EnumFormat` pod offers that we can use.
pub fn enum_modes(bytes: &[u8]) -> Vec<Mode> {
    let Some(p) = parse(bytes) else {
        return Vec::new();
    };
    let mut modes = Vec::new();
    for &encoding in &p.encodings {
        for size in &p.sizes {
            for rate in &p.rates {
                if size.width == 0 || size.height == 0 || rate.num == 0 || rate.denom == 0 {
                    continue;
                }
                modes.push(Mode {
                    encoding,
                    width: size.width,
                    height: size.height,
                    fps: (rate.num, rate.denom),
                });
            }
        }
    }
    modes
}

/// The format the server settled on, with the colour of raw pictures:
/// what the pod says, else BT.601 limited (V4L2's default for webcams).
/// MJPEG is always BT.601 full range (JFIF).
pub fn negotiated(bytes: &[u8]) -> Option<(Mode, Matrix, Range)> {
    let p = parse(bytes)?;
    let encoding = *p.encodings.first()?;
    let size = p.sizes.first().filter(|s| s.width != 0 && s.height != 0)?;
    // The rate is informative only: a format without one still streams.
    let rate = p.rates.first().map_or((0, 1), |r| (r.num, r.denom));
    let mode = Mode {
        encoding,
        width: size.width,
        height: size.height,
        fps: rate,
    };
    if encoding == Encoding::Mjpeg {
        return Some((mode, Matrix::Bt601, Range::Full));
    }
    let matrix = match p.matrix {
        Some(sys::SPA_VIDEO_COLOR_MATRIX_BT709) => Matrix::Bt709,
        Some(sys::SPA_VIDEO_COLOR_MATRIX_BT2020) => Matrix::Bt2020,
        _ => Matrix::Bt601,
    };
    let range = match p.range {
        Some(sys::SPA_VIDEO_COLOR_RANGE_0_255) => Range::Full,
        _ => Range::Limited,
    };
    Some((mode, matrix, range))
}

/// Pixels of 1280x720: the size from which MJPEG beats raw at 30 fps
/// (raw 4:2:2 at 720p30 is about 55 MB/s, near USB 2's limit).
const HD_AREA: u64 = 1280 * 720;

/// Which encoding to prefer for a mode: MJPEG at 720p30 and above (USB
/// bandwidth), raw below (no decode at all).
fn encoding_rank(mode: &Mode) -> u8 {
    let hd = u64::from(mode.width) * u64::from(mode.height) >= HD_AREA && mode.millifps() >= 29_500;
    match (hd, mode.encoding) {
        (true, Encoding::Mjpeg) | (false, Encoding::Nv12) => 2,
        (true, Encoding::Nv12) | (false, Encoding::Yuy2) => 1,
        (true, Encoding::Yuy2) | (false, Encoding::Mjpeg) => 0,
    }
}

/// Picks the mode to ask for: the biggest one within `max` that reaches
/// `fps` (29.97 counts as 30), at the lowest rate that does; failing that,
/// the fastest (then the biggest). Bigger than `max` only when nothing fits (the smallest
/// then). Between encodings of the same mode, see [`encoding_rank`].
pub fn select(modes: &[Mode], max: (u32, u32), fps: u32) -> Option<Mode> {
    let want = u64::from(fps) * 1000 - 500;
    modes.iter().copied().max_by_key(|m| {
        let area = u64::from(m.width) * u64::from(m.height);
        let fits = m.width <= max.0 && m.height <= max.1;
        let fast = m.millifps() >= want;
        (
            fits,
            fast,
            // Too slow: the rate first, 5 fps 1080p is no webcam picture.
            if fast { 0 } else { m.millifps() },
            if fits { area as i64 } else { -(area as i64) },
            encoding_rank(m),
            if fast { -(m.millifps() as i64) } else { 0 },
        )
    })
}

/// The `EnumFormat` pod that asks for exactly `mode`.
pub fn format_pod(mode: &Mode) -> Vec<u8> {
    let mut properties = vec![Property::new(
        sys::SPA_FORMAT_mediaType,
        Value::Id(Id(sys::SPA_MEDIA_TYPE_video)),
    )];
    let raw = |format: VideoFormat| {
        [
            Property::new(
                sys::SPA_FORMAT_mediaSubtype,
                Value::Id(Id(sys::SPA_MEDIA_SUBTYPE_raw)),
            ),
            Property::new(sys::SPA_FORMAT_VIDEO_format, Value::Id(Id(format.as_raw()))),
        ]
    };
    match mode.encoding {
        Encoding::Mjpeg => properties.push(Property::new(
            sys::SPA_FORMAT_mediaSubtype,
            Value::Id(Id(sys::SPA_MEDIA_SUBTYPE_mjpg)),
        )),
        Encoding::Yuy2 => properties.extend(raw(VideoFormat::YUY2)),
        Encoding::Nv12 => properties.extend(raw(VideoFormat::NV12)),
    }
    properties.push(Property::new(
        sys::SPA_FORMAT_VIDEO_size,
        Value::Rectangle(Rectangle {
            width: mode.width,
            height: mode.height,
        }),
    ));
    properties.push(Property::new(
        sys::SPA_FORMAT_VIDEO_framerate,
        Value::Fraction(Fraction {
            num: mode.fps.0,
            denom: mode.fps.1,
        }),
    ));
    serialize(Object {
        type_: sys::SPA_TYPE_OBJECT_Format,
        id: sys::SPA_PARAM_EnumFormat,
        properties,
    })
}

/// The `Buffers` param: shared memory, mapped by the stream.
pub fn buffers_param() -> Vec<u8> {
    let shm = (1 << sys::SPA_DATA_MemPtr) | (1 << sys::SPA_DATA_MemFd);
    serialize(Object {
        type_: sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: sys::SPA_PARAM_Buffers,
        properties: vec![Property::new(
            sys::SPA_PARAM_BUFFERS_dataType,
            Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: shm,
                    flags: vec![],
                },
            ))),
        )],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn mode(encoding: Encoding, width: u32, height: u32, fps: u32) -> Mode {
        Mode {
            encoding,
            width,
            height,
            fps: (fps, 1),
        }
    }

    /// An `EnumFormat` as the V4L2 node sends it: one fixed size, the rates
    /// as an enum choice.
    fn v4l2_pod(
        subtype: u32,
        format: Option<VideoFormat>,
        w: u32,
        h: u32,
        rates: &[u32],
    ) -> Vec<u8> {
        let mut properties = vec![
            Property::new(
                sys::SPA_FORMAT_mediaType,
                Value::Id(Id(sys::SPA_MEDIA_TYPE_video)),
            ),
            Property::new(sys::SPA_FORMAT_mediaSubtype, Value::Id(Id(subtype))),
        ];
        if let Some(f) = format {
            properties.push(Property::new(
                sys::SPA_FORMAT_VIDEO_format,
                Value::Id(Id(f.as_raw())),
            ));
        }
        let frac = |n| Fraction { num: n, denom: 1 };
        properties.push(Property::new(
            sys::SPA_FORMAT_VIDEO_size,
            Value::Rectangle(Rectangle {
                width: w,
                height: h,
            }),
        ));
        properties.push(Property::new(
            sys::SPA_FORMAT_VIDEO_framerate,
            Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: frac(rates[0]),
                    alternatives: rates.iter().map(|&r| frac(r)).collect(),
                },
            ))),
        ));
        serialize(Object {
            type_: sys::SPA_TYPE_OBJECT_Format,
            id: sys::SPA_PARAM_EnumFormat,
            properties,
        })
    }

    /// A typical UVC webcam: raw only up to 480p30, 1080p raw at 5 fps.
    fn uvc() -> Vec<Mode> {
        let pods = [
            v4l2_pod(
                sys::SPA_MEDIA_SUBTYPE_raw,
                Some(VideoFormat::YUY2),
                640,
                480,
                &[30, 15],
            ),
            v4l2_pod(
                sys::SPA_MEDIA_SUBTYPE_raw,
                Some(VideoFormat::YUY2),
                1920,
                1080,
                &[5],
            ),
            v4l2_pod(sys::SPA_MEDIA_SUBTYPE_mjpg, None, 640, 480, &[30]),
            v4l2_pod(sys::SPA_MEDIA_SUBTYPE_mjpg, None, 1280, 720, &[60, 30]),
            v4l2_pod(sys::SPA_MEDIA_SUBTYPE_mjpg, None, 1920, 1080, &[30]),
            v4l2_pod(sys::SPA_MEDIA_SUBTYPE_mjpg, None, 3840, 2160, &[15]),
            // H.264 and unsupported raw formats are skipped.
            v4l2_pod(sys::SPA_MEDIA_SUBTYPE_h264, None, 1920, 1080, &[30]),
            v4l2_pod(
                sys::SPA_MEDIA_SUBTYPE_raw,
                Some(VideoFormat::RGB),
                640,
                480,
                &[30],
            ),
        ];
        pods.iter().flat_map(|p| enum_modes(p)).collect()
    }

    #[test]
    fn enum_modes_expands_each_rate_and_skips_unusable_formats() {
        let modes = uvc();
        assert_eq!(modes.len(), 8, "{modes:?}");
        assert!(modes.contains(&mode(Encoding::Yuy2, 640, 480, 15)));
        assert!(modes.contains(&mode(Encoding::Mjpeg, 1280, 720, 60)));
        assert!(modes.iter().all(|m| m.width != 0 && m.fps.1 == 1));
    }

    #[test]
    fn hd_at_30_fps_prefers_mjpeg() {
        assert_eq!(
            select(&uvc(), (1920, 1080), 30),
            Some(mode(Encoding::Mjpeg, 1920, 1080, 30))
        );
        // 720p: 30 fps is enough, 60 would cost decode time for nothing.
        assert_eq!(
            select(&uvc(), (1280, 720), 30),
            Some(mode(Encoding::Mjpeg, 1280, 720, 30))
        );
    }

    #[test]
    fn small_sizes_prefer_raw_and_nv12_over_yuy2() {
        assert_eq!(
            select(&uvc(), (640, 480), 30),
            Some(mode(Encoding::Yuy2, 640, 480, 30))
        );
        let both = [
            mode(Encoding::Yuy2, 640, 480, 30),
            mode(Encoding::Mjpeg, 640, 480, 30),
            mode(Encoding::Nv12, 640, 480, 30),
        ];
        assert_eq!(
            select(&both, (1920, 1080), 30),
            Some(mode(Encoding::Nv12, 640, 480, 30))
        );
        // At 720p30 raw NV12 is only second to MJPEG.
        let hd = [
            mode(Encoding::Yuy2, 1280, 720, 30),
            mode(Encoding::Nv12, 1280, 720, 30),
        ];
        assert_eq!(select(&hd, (1920, 1080), 30), Some(hd[1]));
    }

    #[test]
    fn frame_rate_beats_size_and_ntsc_rates_count() {
        // 1080p raw at 5 fps loses to 480p at 30.
        let modes = [
            mode(Encoding::Yuy2, 1920, 1080, 5),
            mode(Encoding::Yuy2, 640, 480, 30),
        ];
        assert_eq!(select(&modes, (1920, 1080), 30), Some(modes[1]));
        let ntsc = Mode {
            fps: (30000, 1001),
            ..mode(Encoding::Mjpeg, 1920, 1080, 0)
        };
        assert_eq!(
            select(
                &[ntsc, mode(Encoding::Mjpeg, 1280, 720, 30)],
                (1920, 1080),
                30
            ),
            Some(ntsc)
        );
        // Nothing reaches the rate: the fastest of the biggest.
        let slow = [
            mode(Encoding::Mjpeg, 1920, 1080, 10),
            mode(Encoding::Mjpeg, 1920, 1080, 15),
        ];
        assert_eq!(select(&slow, (1920, 1080), 30), Some(slow[1]));
        // A 25 fps camera: MJPEG at 25 beats raw at 5, whatever the size.
        let pal = [
            mode(Encoding::Yuy2, 1920, 1080, 5),
            mode(Encoding::Mjpeg, 1920, 1080, 25),
            mode(Encoding::Mjpeg, 1280, 720, 25),
        ];
        assert_eq!(select(&pal, (1920, 1080), 30), Some(pal[1]));
        assert_eq!(
            select(
                &pal[..1]
                    .iter()
                    .chain(&pal[2..])
                    .copied()
                    .collect::<Vec<_>>(),
                (1920, 1080),
                30
            ),
            Some(pal[2])
        );
    }

    #[test]
    fn too_big_only_when_nothing_fits() {
        let modes = [
            mode(Encoding::Mjpeg, 3840, 2160, 30),
            mode(Encoding::Mjpeg, 2560, 1440, 30),
        ];
        assert_eq!(select(&modes, (1920, 1080), 30), Some(modes[1]));
        assert_eq!(select(&[], (1920, 1080), 30), None);
    }

    #[test]
    fn format_pod_round_trips_and_mjpeg_is_full_range_bt601() {
        for m in [
            mode(Encoding::Mjpeg, 1920, 1080, 30),
            mode(Encoding::Yuy2, 640, 480, 30),
            mode(Encoding::Nv12, 1280, 720, 60),
        ] {
            assert_eq!(enum_modes(&format_pod(&m)), vec![m]);
            let (got, matrix, range) = negotiated(&format_pod(&m)).unwrap();
            assert_eq!(got, m);
            assert_eq!(matrix, Matrix::Bt601);
            let full = m.encoding == Encoding::Mjpeg;
            assert_eq!(range, if full { Range::Full } else { Range::Limited });
        }
    }

    #[test]
    fn negotiated_reads_the_raw_colour() {
        let pod = |matrix, range| {
            let mut props = vec![
                Property::new(
                    sys::SPA_FORMAT_mediaType,
                    Value::Id(Id(sys::SPA_MEDIA_TYPE_video)),
                ),
                Property::new(
                    sys::SPA_FORMAT_mediaSubtype,
                    Value::Id(Id(sys::SPA_MEDIA_SUBTYPE_raw)),
                ),
                Property::new(
                    sys::SPA_FORMAT_VIDEO_format,
                    Value::Id(Id(VideoFormat::NV12.as_raw())),
                ),
                Property::new(
                    sys::SPA_FORMAT_VIDEO_size,
                    Value::Rectangle(Rectangle {
                        width: 640,
                        height: 480,
                    }),
                ),
            ];
            props.push(Property::new(
                sys::SPA_FORMAT_VIDEO_colorMatrix,
                Value::Id(Id(matrix)),
            ));
            props.push(Property::new(
                sys::SPA_FORMAT_VIDEO_colorRange,
                Value::Id(Id(range)),
            ));
            serialize(Object {
                type_: sys::SPA_TYPE_OBJECT_Format,
                id: sys::SPA_PARAM_Format,
                properties: props,
            })
        };
        let (m, matrix, range) = negotiated(&pod(
            sys::SPA_VIDEO_COLOR_MATRIX_BT709,
            sys::SPA_VIDEO_COLOR_RANGE_0_255,
        ))
        .unwrap();
        assert_eq!((m.encoding, m.width, m.fps), (Encoding::Nv12, 640, (0, 1)));
        assert_eq!((matrix, range), (Matrix::Bt709, Range::Full));
        // A zero size is no format.
        let empty = format_pod(&mode(Encoding::Yuy2, 640, 0, 30));
        assert!(negotiated(&empty).is_none());
        let (_, matrix, range) = negotiated(&pod(
            sys::SPA_VIDEO_COLOR_MATRIX_UNKNOWN,
            sys::SPA_VIDEO_COLOR_RANGE_UNKNOWN,
        ))
        .unwrap();
        assert_eq!((matrix, range), (Matrix::Bt601, Range::Limited));
    }

    #[test]
    fn fixed_values_wrapped_in_none_choices_are_read() {
        let none = |v| {
            Value::Choice(ChoiceValue::Id(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::None(Id(v)),
            )))
        };
        let pod = serialize(Object {
            type_: sys::SPA_TYPE_OBJECT_Format,
            id: sys::SPA_PARAM_Format,
            properties: vec![
                Property::new(sys::SPA_FORMAT_mediaType, none(sys::SPA_MEDIA_TYPE_video)),
                Property::new(
                    sys::SPA_FORMAT_mediaSubtype,
                    none(sys::SPA_MEDIA_SUBTYPE_mjpg),
                ),
                Property::new(
                    sys::SPA_FORMAT_VIDEO_size,
                    Value::Choice(ChoiceValue::Rectangle(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::None(Rectangle {
                            width: 1280,
                            height: 720,
                        }),
                    ))),
                ),
                Property::new(
                    sys::SPA_FORMAT_VIDEO_framerate,
                    Value::Choice(ChoiceValue::Fraction(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::None(Fraction { num: 30, denom: 1 }),
                    ))),
                ),
            ],
        });
        let (m, matrix, range) = negotiated(&pod).unwrap();
        assert_eq!(m, mode(Encoding::Mjpeg, 1280, 720, 30));
        assert_eq!((matrix, range), (Matrix::Bt601, Range::Full));
    }

    #[test]
    fn audio_and_garbage_pods_give_nothing() {
        assert!(enum_modes(&[]).is_empty());
        assert!(enum_modes(&[1, 2, 3, 4, 5, 6, 7, 8]).is_empty());
        assert!(negotiated(&crate::format::header_meta_param()).is_none());
    }
}
