//! Camera checks for the robustness lab (`scripts/robustness/c1_camera.sh`),
//! never for a real session: the lab's PipeWire has no real device.
//!
//! - `camera_lab fake mjpeg <file.jpg> [WxH]` / `camera_lab fake yuy2`: a
//!   fake camera node (`lab_camera`) sending that picture at 30 fps (MJPEG
//!   announced as 1280x720 unless told).
//! - `camera_lab list`: the cameras found.
//! - `camera_lab open <node.name> <seconds>`: streams it, prints what came.

use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Property, Value};
use pw::spa::sys;
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
use vesper_capture::camera::{self, Camera, CameraFrame, CpuLayout, Encoding, Mode};

fn main() -> vesper_capture::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["list"] => {
            for c in camera::cameras()? {
                println!("camera {} ({})", c.node_name, c.description);
            }
            Ok(())
        }
        ["open", node, secs] => open(node, secs.parse()?),
        ["fake", "mjpeg", file] => fake(mjpeg(1280, 720), std::fs::read(file)?),
        ["fake", "mjpeg", file, size] => {
            let (w, h) = size.split_once('x').ok_or("size: WxH")?;
            fake(mjpeg(w.parse()?, h.parse()?), std::fs::read(file)?)
        }
        ["fake", "yuy2"] => {
            let (w, h) = (640usize, 480usize);
            // BT.601 limited (Y 81, U 90, V 240): pure red.
            let picture = [81u8, 90, 81, 240].repeat(w / 2 * h);
            fake(
                Mode {
                    encoding: Encoding::Yuy2,
                    width: w as u32,
                    height: h as u32,
                    fps: (30, 1),
                },
                picture,
            )
        }
        _ => Err(
            "usage: camera_lab list | open <node> <secs> | fake mjpeg <jpg> [WxH] | fake yuy2"
                .into(),
        ),
    }
}

fn open(node: &str, secs: u64) -> vesper_capture::Result<()> {
    let cam = Camera::open(node)?;
    let end = Instant::now() + Duration::from_secs(secs);
    let (mut gpu, mut cpu, mut first) = (0u32, 0u32, true);
    while Instant::now() < end {
        match cam.take() {
            Ok(Some(frame)) => {
                if first {
                    first = false;
                    describe(&frame);
                }
                match frame {
                    CameraFrame::Gpu(_) => gpu += 1,
                    CameraFrame::Cpu(_) => cpu += 1,
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                println!("ended: {e}");
                break;
            }
        }
    }
    println!(
        "mode {:?}, {gpu} GPU and {cpu} CPU frames, software decode {}",
        cam.mode(),
        cam.software_decode()
    );
    let closing = Instant::now();
    drop(cam);
    println!("closed in {:?}", closing.elapsed());
    Ok(())
}

fn mjpeg(width: u32, height: u32) -> Mode {
    Mode {
        encoding: Encoding::Mjpeg,
        width,
        height,
        fps: (30, 1),
    }
}

fn describe(frame: &CameraFrame) {
    match frame {
        CameraFrame::Gpu(f) => println!(
            "first frame: GPU {}x{} {:?} modifier {:#x} {} planes",
            f.width,
            f.height,
            std::str::from_utf8(&f.fourcc.to_le_bytes()).unwrap_or("?"),
            f.modifier,
            f.planes.len()
        ),
        CameraFrame::Cpu(f) => {
            let p = f.planes[0];
            let at = p.offset + (f.height as usize / 2) * p.stride + f.width as usize;
            let px = match f.layout {
                CpuLayout::Yuy2 => &f.data[at..at + 4],
                CpuLayout::Nv12 => &f.data[at..at + 1],
            };
            println!(
                "first frame: CPU {}x{} {:?} {:?} {:?}, middle {px:?}",
                f.width, f.height, f.layout, f.matrix, f.range
            );
        }
    }
}

fn serialize(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("pod into a Vec")
        .0
        .into_inner()
}

/// A fake camera node driving the graph at 30 fps with `picture`.
fn fake(mode: Mode, picture: Vec<u8>) -> vesper_capture::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "lab-camera",
        pw::properties::properties! {
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::NODE_NAME => "lab_camera",
            *pw::keys::NODE_DESCRIPTION => "Lab Camera",
            // What makes it a camera rather than a screencast stream.
            *pw::keys::DEVICE_ID => "999",
        },
    )?;
    let stride = match mode.encoding {
        Encoding::Yuy2 => mode.width as i32 * 2,
        _ => 0,
    };
    let size = picture.len().next_power_of_two() as i32;
    let _listener = stream
        .add_local_listener_with_user_data(picture)
        .param_changed(move |stream, _, id, pod| {
            if id != pw::spa::param::ParamType::Format.as_raw() || pod.is_none() {
                return;
            }
            let int = |v| Value::Int(v);
            let buffers = serialize(Object {
                type_: sys::SPA_TYPE_OBJECT_ParamBuffers,
                id: sys::SPA_PARAM_Buffers,
                properties: vec![
                    Property::new(sys::SPA_PARAM_BUFFERS_buffers, int(4)),
                    Property::new(sys::SPA_PARAM_BUFFERS_blocks, int(1)),
                    Property::new(sys::SPA_PARAM_BUFFERS_size, int(size)),
                    Property::new(sys::SPA_PARAM_BUFFERS_stride, int(stride)),
                    Property::new(
                        sys::SPA_PARAM_BUFFERS_dataType,
                        Value::Choice(pw::spa::pod::ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Flags {
                                default: 1 << sys::SPA_DATA_MemFd,
                                flags: vec![],
                            },
                        ))),
                    ),
                ],
            });
            let mut pods: Vec<&Pod> = Pod::from_bytes(&buffers).into_iter().collect();
            if let Err(e) = stream.update_params(&mut pods) {
                eprintln!("update_params: {e}");
            }
        })
        .process(move |stream, picture| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let n = match data.data() {
                Some(dst) if dst.len() >= picture.len() => {
                    dst[..picture.len()].copy_from_slice(picture);
                    picture.len()
                }
                _ => 0,
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.size_mut() = n as u32;
            *chunk.stride_mut() = stride;
        })
        .register()?;
    let pod = camera::modes::format_pod(&mode);
    let mut pods: Vec<&Pod> = Pod::from_bytes(&pod).into_iter().collect();
    stream.connect(
        pw::spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut pods,
    )?;
    let timer = mainloop.loop_().add_timer({
        let stream = stream.clone();
        move |_| {
            // The crate's wrapper needs its v0_3_34 feature; the call is the same.
            unsafe { pw::sys::pw_stream_trigger_process(stream.as_raw_ptr()) };
        }
    });
    let frame = Duration::from_millis(33);
    timer.update_timer(Some(frame), Some(frame)).into_result()?;
    println!("fake camera {mode:?} up");
    mainloop.run();
    Ok(())
}
