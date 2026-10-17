use std::ffi::c_int;
use std::io::Seek;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::ptr;

use ffmpeg_next::ffi as ff;
use pw::main_loop::MainLoopRc;

use super::*;

#[test]
fn only_device_video_sources_are_cameras() {
    let cam = camera_info(
        Some("Video/Source"),
        Some("42"),
        Some("v4l2_input.pci-0000_0c_00.3-usb-0_1_1.0"),
        Some("HD Pro Webcam C920 (V4L2)"),
        None,
    );
    assert_eq!(
        cam,
        Some(CameraInfo {
            node_name: "v4l2_input.pci-0000_0c_00.3-usb-0_1_1.0".into(),
            description: "HD Pro Webcam C920 (V4L2)".into(),
        })
    );
    // No description: the nick, else the name.
    let nick = camera_info(
        Some("Video/Source"),
        Some("43"),
        Some("n"),
        Some(""),
        Some("Cam"),
    );
    assert_eq!(nick.unwrap().description, "Cam");
    let bare = camera_info(Some("Video/Source"), Some("42"), Some("n"), None, None);
    assert_eq!(bare.unwrap().description, "n");
    // A screencast stream, a sink, a nameless node: not cameras.
    assert_eq!(
        camera_info(Some("Video/Source"), None, Some("kwin"), None, None),
        None
    );
    assert_eq!(
        camera_info(Some("Video/Sink"), Some("42"), Some("n"), None, None),
        None
    );
    assert_eq!(
        camera_info(Some("Video/Source"), Some("42"), Some(""), None, None),
        None
    );
}

const fn mode(encoding: Encoding, width: u32, height: u32) -> Mode {
    Mode {
        encoding,
        width,
        height,
        fps: (30, 1),
    }
}

#[test]
fn raw_frames_are_copied_with_checked_planes() {
    // YUY2 4x2 with padded rows.
    let yuy2: Vec<u8> = (0..2 * 20).map(|i| i as u8).collect();
    let (layout, planes, data) = raw_frame(mode(Encoding::Yuy2, 4, 2), &[(&yuy2, 20)]).unwrap();
    assert_eq!(layout, CpuLayout::Yuy2);
    assert_eq!(
        planes,
        [CpuPlane {
            offset: 0,
            stride: 20
        }]
    );
    assert_eq!(data, yuy2);
    // Stride smaller than a row, or a short buffer: refused.
    assert!(raw_frame(mode(Encoding::Yuy2, 4, 2), &[(&yuy2, 4)]).is_none());
    assert!(raw_frame(mode(Encoding::Yuy2, 4, 2), &[(&yuy2[..27], 20)]).is_none());
    // The last row needs no padding.
    assert!(raw_frame(mode(Encoding::Yuy2, 4, 2), &[(&yuy2[..28], 20)]).is_some());

    // NV12 4x4 in one data: UV right after Y. A zero stride means packed.
    let nv12 = [7u8; 16 + 8];
    let (layout, planes, _) = raw_frame(mode(Encoding::Nv12, 4, 4), &[(&nv12, 0)]).unwrap();
    assert_eq!(layout, CpuLayout::Nv12);
    assert_eq!(
        planes,
        [
            CpuPlane {
                offset: 0,
                stride: 4
            },
            CpuPlane {
                offset: 16,
                stride: 4
            }
        ]
    );
    assert!(raw_frame(mode(Encoding::Nv12, 4, 4), &[(&nv12[..23], 4)]).is_none());
    // One data per plane.
    let (y, uv) = ([1u8; 32], [2u8; 16]);
    let (_, planes, data) = raw_frame(mode(Encoding::Nv12, 4, 4), &[(&y, 8), (&uv, 8)]).unwrap();
    assert_eq!(
        planes,
        [
            CpuPlane {
                offset: 0,
                stride: 8
            },
            CpuPlane {
                offset: 28,
                stride: 8
            }
        ]
    );
    assert_eq!(data.len(), 28 + 12);
    // One data too short for UV after Y.
    assert!(raw_frame(mode(Encoding::Nv12, 4, 4), &[(&y, 8)]).is_none());
    assert!(raw_frame(mode(Encoding::Nv12, 4, 4), &[(&y, 8), (&uv, 8), (&uv, 8)]).is_none());
}

/// A solid `rgb` JPEG of `w`x`h` in `sampling` (`yuvj422p`, `yuvj420p`)
/// made by the ffmpeg CLI, or `None` without it.
fn jpeg(rgb: u32, w: u32, h: u32, sampling: &str) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("color=c=0x{rgb:06x}:s={w}x{h}"))
        .args(["-frames:v", "1", "-pix_fmt", sampling, "-q:v", "2"])
        .args(["-c:v", "mjpeg", "-f", "image2pipe", "-"])
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "ffmpeg: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(out.stdout)
}

fn close(got: &[u8], want: &[u8], tolerance: u8) -> bool {
    got.iter()
        .zip(want)
        .all(|(g, w)| g.abs_diff(*w) <= tolerance)
}

/// RGB (192, 48, 32) in BT.601 full range.
const RGB: u32 = 0xc03020;
const YUV: [u8; 3] = [89, 96, 201];

#[test]
fn cpu_fallback_repacks_jpeg_samples_as_bt601_full_range() {
    let pts = Duration::from_millis(1234);
    for (sampling, layout) in [("yuvj422p", CpuLayout::Yuy2), ("yuvj420p", CpuLayout::Nv12)] {
        // Odd sizes: the last chroma column and row must be there too.
        let Some(jpeg) = jpeg(RGB, 641, 361, sampling) else {
            eprintln!("no ffmpeg CLI, skipping");
            return;
        };
        let mut dec = decode::Decoder::cpu().unwrap();
        assert!(!dec.is_gpu());
        let CameraFrame::Cpu(f) = dec.decode(&jpeg, pts).unwrap() else {
            panic!("the CPU decoder made a GPU frame");
        };
        assert_eq!(
            (f.width, f.height, f.layout, f.pts),
            (641, 361, layout, pts)
        );
        assert_eq!((f.matrix, f.range), (Matrix::Bt601, Range::Full));
        let (w, h) = (641usize, 361usize);
        let last = &f.planes[f.planes.len() - 1];
        let rows = if layout == CpuLayout::Nv12 {
            h.div_ceil(2)
        } else {
            h
        };
        assert_eq!(f.data.len(), last.offset + last.stride * rows);
        // A pixel in the middle and the bottom-right one.
        for (x, y) in [((w / 2) & !1, (h / 2) & !1), (w - 1, h - 1)] {
            let p = match layout {
                CpuLayout::Yuy2 => {
                    let s = &f.data[y * f.planes[0].stride + x / 2 * 4..];
                    [s[0], s[1], s[3]]
                }
                CpuLayout::Nv12 => {
                    let uv = &f.data[f.planes[1].offset + y / 2 * f.planes[1].stride + x / 2 * 2..];
                    [f.data[y * f.planes[0].stride + x], uv[0], uv[1]]
                }
            };
            assert!(
                close(&p, &YUV, 3),
                "{sampling} at {x},{y}: {p:?}, want {YUV:?}"
            );
        }
    }
}

/// Reads the exported frame back through a VA import of its DMA-BUF.
unsafe fn read_back(frame: &VideoFrame) -> Vec<u8> {
    let ok =
        |ret: c_int, what: &str| assert!(ret >= 0, "{what}: {}", ffmpeg_next::Error::from(ret));
    unsafe {
        let mut dev = ptr::null_mut();
        ok(
            ff::av_hwdevice_ctx_create(
                &mut dev,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                c"/dev/dri/renderD128".as_ptr(),
                ptr::null_mut(),
                0,
            ),
            "device",
        );
        let mut frames = ff::av_hwframe_ctx_alloc(dev);
        let fc = (*frames).data as *mut ff::AVHWFramesContext;
        (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_BGR0;
        (*fc).width = frame.width as c_int;
        (*fc).height = frame.height as c_int;
        ok(ff::av_hwframe_ctx_init(frames), "frames");

        let mut src = ff::av_frame_alloc();
        let desc_buf = ff::av_buffer_allocz(size_of::<ff::AVDRMFrameDescriptor>());
        (*src).buf[0] = desc_buf;
        (*src).data[0] = (*desc_buf).data;
        let desc = &mut *((*desc_buf).data as *mut ff::AVDRMFrameDescriptor);
        let fd = frame.planes[0].fd.as_raw_fd();
        desc.nb_objects = 1;
        desc.objects[0].fd = fd;
        desc.objects[0].size = std::fs::File::from(frame.planes[0].fd.try_clone().unwrap())
            .seek(std::io::SeekFrom::End(0))
            .unwrap() as usize;
        desc.objects[0].format_modifier = frame.modifier;
        desc.nb_layers = 1;
        desc.layers[0].format = frame.fourcc;
        desc.layers[0].nb_planes = frame.planes.len() as c_int;
        for (dst, p) in desc.layers[0].planes.iter_mut().zip(&frame.planes) {
            dst.offset = p.offset as isize;
            dst.pitch = p.stride as isize;
        }
        (*src).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
        (*src).width = frame.width as c_int;
        (*src).height = frame.height as c_int;
        let mut va = ff::av_frame_alloc();
        (*va).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
        (*va).hw_frames_ctx = ff::av_buffer_ref(frames);
        ok(
            ff::av_hwframe_map(va, src, ff::AV_HWFRAME_MAP_READ as c_int),
            "import",
        );
        let mut sw = ff::av_frame_alloc();
        (*sw).format = ff::AVPixelFormat::AV_PIX_FMT_BGR0 as c_int;
        ok(ff::av_hwframe_transfer_data(sw, va, 0), "read back");
        let stride = (*sw).linesize[0] as usize;
        let pixels = std::slice::from_raw_parts((*sw).data[0], stride * frame.height as usize);
        let mut out = Vec::with_capacity(frame.width as usize * frame.height as usize * 4);
        for row in pixels.chunks_exact(stride) {
            out.extend_from_slice(&row[..frame.width as usize * 4]);
        }
        // The import holds the BO; `src` only borrows the frame's fd.
        desc.objects[0].fd = -1;
        ff::av_frame_free(&mut sw);
        ff::av_frame_free(&mut va);
        ff::av_frame_free(&mut src);
        ff::av_buffer_unref(&mut frames);
        ff::av_buffer_unref(&mut dev);
        out
    }
}

#[test]
fn gpu_decodes_mjpeg_to_an_exported_rgb_frame() {
    if !vesper_core::test_gpu() {
        return;
    }
    let mut dec = decode::Decoder::gpu().expect("VAAPI on renderD128");
    let pts = Duration::from_millis(40);
    let mut seen = Vec::new();
    for (i, (w, h, sampling)) in [
        (1280, 720, "yuvj422p"),
        (1280, 720, "yuvj420p"),
        (1280, 720, "yuvj420p"),
        (1920, 1080, "yuvj422p"),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(jpeg) = jpeg(RGB, w, h, sampling) else {
            eprintln!("no ffmpeg CLI, skipping");
            return;
        };
        let frame = match dec.decode(&jpeg, pts * i as u32) {
            Ok(CameraFrame::Gpu(f)) => f,
            Ok(CameraFrame::Cpu(_)) => panic!("the GPU decoder made a CPU frame"),
            Err(e) => panic!("{sampling} {w}x{h}: {e}"),
        };
        assert_eq!(
            (frame.width, frame.height, frame.pts),
            (w, h, pts * i as u32)
        );
        // XR24 or AR24: what the encoder and the compositor import.
        let fourcc = frame.fourcc.to_le_bytes();
        assert!(
            &fourcc == b"XR24" || &fourcc == b"AR24",
            "{:?}",
            std::str::from_utf8(&fourcc)
        );
        let pixels = unsafe { read_back(&frame) };
        for (x, y) in [(w / 2, h / 2), (0, 0), (w - 1, h - 1)] {
            let px = &pixels[(y * w + x) as usize * 4..][..3];
            // BGRx in memory.
            assert!(
                close(px, &[32, 48, 192], 6),
                "{sampling} {w}x{h} at {x},{y}: {px:?}"
            );
        }
        seen.push((frame.pool_generation, frame.buffer_id));
    }
    // A new decoder pool (other sampling or size) is a new RGB pool: the
    // generation moves on; the same picture again keeps it.
    assert_ne!(seen[0].0, seen[1].0);
    assert_eq!(seen[1].0, seen[2].0);
    assert_ne!(seen[2].0, seen[3].0);
}

#[test]
fn gpu_surfaces_keep_their_buffer_id_and_return_on_release() {
    if !vesper_core::test_gpu() {
        return;
    }
    let Some(jpeg) = jpeg(RGB, 640, 480, "yuvj422p") else {
        eprintln!("no ffmpeg CLI, skipping");
        return;
    };
    let mut dec = decode::Decoder::gpu().unwrap();
    let mut ids = std::collections::BTreeSet::new();
    // Many more pictures than any pool holds, each released at once: the
    // pool cycles through a few surfaces, ids stay small and stable.
    for i in 0..40 {
        let CameraFrame::Gpu(f) = dec.decode(&jpeg, Duration::from_millis(i)).unwrap() else {
            panic!("not a GPU frame");
        };
        ids.insert(f.buffer_id);
    }
    assert!(ids.len() <= 4, "{ids:?}");
    // Frames held by the consumer keep their surface: distinct ids.
    let held: Vec<VideoFrame> = (0..3)
        .map(
            |i| match dec.decode(&jpeg, Duration::from_millis(i)).unwrap() {
                CameraFrame::Gpu(f) => f,
                CameraFrame::Cpu(_) => panic!("not a GPU frame"),
            },
        )
        .collect();
    let distinct: std::collections::BTreeSet<_> = held.iter().map(|f| f.buffer_id).collect();
    assert_eq!(distinct.len(), held.len());
    // A reopened camera (new decoder) never reuses a generation.
    let mut again = decode::Decoder::gpu().unwrap();
    let CameraFrame::Gpu(f) = again.decode(&jpeg, Duration::ZERO).unwrap() else {
        panic!("not a GPU frame");
    };
    assert!(held.iter().all(|h| h.pool_generation != f.pool_generation));
}

#[test]
fn cpu_decoder_rejects_444_jpegs_as_unsupported() {
    let Some(jpeg) = jpeg(RGB, 64, 64, "yuvj444p") else {
        eprintln!("no ffmpeg CLI, skipping");
        return;
    };
    let err = decode::Decoder::cpu()
        .unwrap()
        .decode(&jpeg, Duration::ZERO)
        .unwrap_err();
    assert!(err.is::<decode::Unsupported>(), "{err}");
}

/// A `Shared` whose PipeWire side is a bare loop (no server).
fn lifecycle() -> (Arc<Shared>, pw::channel::Receiver<()>) {
    pw::init();
    let (wake, rx) = pw::channel::channel();
    (Arc::new(Shared::new(wake)), rx)
}

/// Whether the PipeWire side gets woken within 5 s.
fn woken(rx: pw::channel::Receiver<()>) -> bool {
    let mainloop = MainLoopRc::new(None).unwrap();
    let woken = Rc::new(Cell::new(false));
    let _attached = rx.attach(mainloop.loop_(), {
        let (woken, mainloop) = (woken.clone(), mainloop.clone());
        move |()| {
            woken.set(true);
            mainloop.quit();
        }
    });
    crate::pw::run_until(&mainloop, || woken.get(), Duration::from_secs(5)).unwrap()
}

/// Runs `decode_loop` on its own thread; the receiver gets `()` when it returns.
fn spawn_decoder(shared: &Arc<Shared>) -> std::sync::mpsc::Receiver<()> {
    let (done, exited) = std::sync::mpsc::channel();
    let shared = shared.clone();
    std::thread::spawn(move || {
        decode_loop(&shared);
        let _ = done.send(());
    });
    exited
}

#[test]
fn a_decode_side_end_stops_the_pipewire_side_and_drops_frames() {
    let (shared, rx) = lifecycle();
    let exited = spawn_decoder(&shared);
    let s = shared.clone();
    // As the decode thread does when the camera sends nothing decodable.
    std::thread::spawn(move || s.end("broken pictures".into()));
    assert!(woken(rx), "the PipeWire side must stop too");
    exited
        .recv_timeout(Duration::from_secs(5))
        .expect("the decode loop exits");
    let slot = shared.lock();
    assert!(slot.stop);
    assert_eq!(slot.ended.as_deref(), Some("broken pictures"));
    drop(slot);
    // Nothing goes out once ended.
    shared.publish(CameraFrame::Cpu(CpuFrame {
        width: 2,
        height: 2,
        layout: CpuLayout::Yuy2,
        planes: vec![CpuPlane {
            offset: 0,
            stride: 4,
        }],
        data: vec![0; 8],
        matrix: Matrix::Bt601,
        range: Range::Limited,
        pts: Duration::ZERO,
    }));
    assert!(shared.lock().frame.is_none());
}

#[test]
fn closing_stops_both_sides_without_an_error() {
    let (shared, rx) = lifecycle();
    let exited = spawn_decoder(&shared);
    // What `Camera`'s drop does.
    assert!(shared.stop().is_none());
    assert!(woken(rx));
    exited
        .recv_timeout(Duration::from_secs(5))
        .expect("the decode loop exits");
    assert!(shared.lock().ended.is_none());
}
