//! `vesper-vst-host`: runs one VST3 effect for Vesper, out of Vesper's
//! process, so a plugin that crashes or hangs only takes this process down.
//!
//! - `vesper-vst-host --serve`: started by Vesper with its control socket on
//!   fd 3 and the shared audio region on fd 4 (see `vesper_vst::host`).
//! - `vesper-vst-host --scan <bundle>`: prints the bundle's audio effects,
//!   one per line (see `vesper_vst::PluginInfo::parse`).

#![allow(non_upper_case_globals, non_snake_case)]

mod com;

use std::ffi::{CString, OsStr, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use vesper_vst::PluginInfo;
use vesper_vst::host::{Loaded, Plugin};
use vesper_vst::protocol::{Load, Param};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper, Interface};

use com::{HostApp, MemoryStream, cstr, wstr};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let args: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
    match args.as_slice() {
        [mode] if *mode == "--serve" => match vesper_vst::host::serve(Vst3::default()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("vesper-vst-host: {e}");
                ExitCode::FAILURE
            }
        },
        [mode, path] if *mode == "--scan" => match scan(Path::new(path)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("vesper-vst-host: {}: {e}", path.display());
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: vesper-vst-host --serve | --scan <bundle.vst3>");
            ExitCode::from(2)
        }
    }
}

/// A loaded plugin module. Never unloaded: the process exits instead.
struct Module {
    factory: ComPtr<IPluginFactory>,
}

impl Module {
    fn open(path: &Path) -> Result<Self, String> {
        let so = vesper_vst::scan::module_path(path);
        let name = CString::new(so.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: dlopen/dlsym with valid C strings; the entry points have
        // the VST3 module signatures.
        unsafe {
            let handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if handle.is_null() {
                let e = libc::dlerror();
                return Err(if e.is_null() {
                    "cannot load".into()
                } else {
                    std::ffi::CStr::from_ptr(e).to_string_lossy().into_owned()
                });
            }
            let entry = libc::dlsym(handle, c"ModuleEntry".as_ptr());
            if !entry.is_null() {
                let entry: unsafe extern "system" fn(*mut c_void) -> bool =
                    std::mem::transmute(entry);
                if !entry(handle) {
                    return Err("ModuleEntry failed".into());
                }
            }
            let get = libc::dlsym(handle, c"GetPluginFactory".as_ptr());
            if get.is_null() {
                return Err("not a VST3 module (no GetPluginFactory)".into());
            }
            let get: unsafe extern "system" fn() -> *mut IPluginFactory = std::mem::transmute(get);
            let factory = ComPtr::from_raw(get()).ok_or("no plugin factory")?;
            Ok(Self { factory })
        }
    }

    /// An instance of class `cid`, as interface `I`.
    fn create<I: Interface>(&self, cid: &[u8; 16]) -> Option<ComPtr<I>> {
        let mut obj = std::ptr::null_mut();
        // SAFETY: valid ids and out pointer; on success `obj` is an owned `I`.
        unsafe {
            let r =
                self.factory
                    .createInstance(cid.as_ptr().cast(), I::IID.as_ptr().cast(), &mut obj);
            if r != kResultOk {
                return None;
            }
            ComPtr::from_raw(obj.cast())
        }
    }
}

const AUDIO_MODULE: &str = "Audio Module Class";

/// Prints the audio effects of the bundle at `path`.
fn scan(path: &Path) -> Result<(), String> {
    let module = Module::open(path)?;
    let f = &module.factory;
    // SAFETY: plain calls into the factory with valid out structs.
    unsafe {
        let mut info: PFactoryInfo = std::mem::zeroed();
        let factory_vendor = if f.getFactoryInfo(&mut info) == kResultOk {
            cstr(&info.vendor)
        } else {
            String::new()
        };
        let f2 = f.cast::<IPluginFactory2>();
        for i in 0..f.countClasses() {
            let plugin = if let Some(f2) = &f2 {
                let mut c: PClassInfo2 = std::mem::zeroed();
                if f2.getClassInfo2(i, &mut c) != kResultOk || cstr(&c.category) != AUDIO_MODULE {
                    continue;
                }
                let vendor = Some(cstr(&c.vendor)).filter(|v| !v.is_empty());
                PluginInfo {
                    path: path.to_owned(),
                    cid: c.cid.map(|b| b as u8),
                    name: cstr(&c.name),
                    vendor: vendor.unwrap_or_else(|| factory_vendor.clone()),
                    category: cstr(&c.subCategories),
                }
            } else {
                let mut c: PClassInfo = std::mem::zeroed();
                if f.getClassInfo(i, &mut c) != kResultOk || cstr(&c.category) != AUDIO_MODULE {
                    continue;
                }
                PluginInfo {
                    path: path.to_owned(),
                    cid: c.cid.map(|b| b as u8),
                    name: cstr(&c.name),
                    vendor: factory_vendor.clone(),
                    category: String::new(),
                }
            };
            println!("{}", plugin.line());
        }
    }
    Ok(())
}

/// The plugin as the control thread holds it.
#[derive(Default)]
struct Vst3 {
    loaded: Option<Open>,
}

struct Open {
    component: ComPtr<IComponent>,
    controller: Option<ComPtr<IEditController>>,
    // Kept alive while the plugin may call it.
    _host: ComWrapper<HostApp>,
    _module: Module,
}

/// Terminates the initialized parts of a plugin whose load failed.
struct Terminate {
    parts: Vec<ComPtr<IPluginBase>>,
    armed: bool,
}

impl Drop for Terminate {
    fn drop(&mut self) {
        if self.armed {
            for p in self.parts.iter().rev() {
                // SAFETY: each part was initialized and is still alive.
                unsafe { p.terminate() };
            }
        }
    }
}

/// Channels of an arrangement (one bit per speaker).
fn channels(arr: SpeakerArrangement) -> usize {
    arr.count_ones() as usize
}

impl Plugin for Vst3 {
    fn load(&mut self, l: &Load) -> Result<Loaded, String> {
        if l.channels != 2 || l.max_frames == 0 {
            return Err("Vesper's side must be stereo".into());
        }
        let module = Module::open(Path::new(OsStr::from_bytes(&l.path)))?;
        let component: ComPtr<IComponent> = module
            .create(&l.cid)
            .ok_or("no such plugin in the bundle")?;
        let host = ComWrapper::new(HostApp::default());
        let context = host.to_com_ptr::<FUnknown>().expect("FUnknown");
        // SAFETY: the VST3 hosting sequence, on this (the "UI") thread.
        unsafe {
            if component.initialize(context.as_ptr()) != kResultOk {
                return Err("the plugin failed to initialize".into());
            }
            let mut terminate = Terminate {
                parts: component.cast::<IPluginBase>().into_iter().collect(),
                armed: true,
            };
            // Its controller: the component itself, or a separate class.
            let mut separate = false;
            let controller = component.cast::<IEditController>().or_else(|| {
                let mut cid: TUID = [0; 16];
                if component.getControllerClassId(&mut cid) != kResultOk {
                    return None;
                }
                let c: ComPtr<IEditController> = module.create(&cid.map(|b| b as u8))?;
                if c.initialize(context.as_ptr()) != kResultOk {
                    return None;
                }
                separate = true;
                Some(c)
            });
            if let Some(c) = controller.as_ref().filter(|_| separate) {
                terminate.parts.extend(c.cast::<IPluginBase>());
            }
            if let Some(c) = &controller {
                if separate
                    && let (Some(a), Some(b)) = (
                        component.cast::<IConnectionPoint>(),
                        c.cast::<IConnectionPoint>(),
                    )
                {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                }
                let handler = host.to_com_ptr::<IComponentHandler>().expect("handler");
                c.setComponentHandler(handler.as_ptr());
            }
            if !l.component.is_empty() {
                component.setState(MemoryStream::reading(&l.component).as_ptr());
                if let Some(c) = &controller {
                    c.setComponentState(MemoryStream::reading(&l.component).as_ptr());
                }
            }
            if let Some(c) = controller.as_ref().filter(|_| !l.controller.is_empty()) {
                c.setState(MemoryStream::reading(&l.controller).as_ptr());
            }
            let processor = component
                .cast::<IAudioProcessor>()
                .ok_or("not an audio processor")?;
            let audio = MediaTypes_::kAudio as MediaType;
            let (input, output) = (
                BusDirections_::kInput as BusDirection,
                BusDirections_::kOutput as BusDirection,
            );
            let (n_in, n_out) = (
                component.getBusCount(audio, input),
                component.getBusCount(audio, output),
            );
            if n_in < 1 || n_out < 1 {
                return Err("not an effect (no audio input or output)".into());
            }
            let arrangement = |dir, i| {
                let mut arr: SpeakerArrangement = 0;
                processor.getBusArrangement(dir, i, &mut arr);
                arr
            };
            // Stereo main buses, else mono; other buses as they are.
            for main in [SpeakerArr::kStereo, SpeakerArr::kMono] {
                let mut ins: Vec<_> = (0..n_in).map(|i| arrangement(input, i)).collect();
                let mut outs: Vec<_> = (0..n_out).map(|i| arrangement(output, i)).collect();
                (ins[0], outs[0]) = (main, main);
                if processor.setBusArrangements(ins.as_mut_ptr(), n_in, outs.as_mut_ptr(), n_out)
                    == kResultTrue
                {
                    break;
                }
            }
            let (ch_in, ch_out) = (
                channels(arrangement(input, 0)),
                channels(arrangement(output, 0)),
            );
            if !(1..=2).contains(&ch_in) || !(1..=2).contains(&ch_out) {
                return Err(format!(
                    "unsupported bus layout ({ch_in} in, {ch_out} out; mono or stereo only)"
                ));
            }
            component.activateBus(audio, input, 0, 1);
            component.activateBus(audio, output, 0, 1);
            if processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32) != kResultOk {
                return Err("no 32-bit float processing".into());
            }
            let mut setup = ProcessSetup {
                processMode: ProcessModes_::kRealtime as i32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: l.max_frames as i32,
                sampleRate: f64::from(l.sample_rate),
            };
            if processor.setupProcessing(&mut setup) != kResultOk {
                return Err("the plugin refused 48 kHz processing".into());
            }
            if component.setActive(1) != kResultOk {
                return Err("the plugin failed to activate".into());
            }
            // Many plugins answer kNotImplemented here: not an error.
            processor.setProcessing(1);
            let params = controller.as_ref().map(params).unwrap_or_default();
            let buses_in: Vec<usize> = (0..n_in).map(|i| channels(arrangement(input, i))).collect();
            let buses_out: Vec<usize> = (0..n_out)
                .map(|i| channels(arrangement(output, i)))
                .collect();
            let audio_side = com::Audio::new(
                processor,
                component.clone(),
                host.latency_changed.clone(),
                l,
                &buses_in,
                &buses_out,
            );
            let name = class_name(&module, &l.cid).unwrap_or_else(|| "VST3 plugin".into());
            terminate.armed = false;
            self.loaded = Some(Open {
                component,
                controller,
                _host: host,
                _module: module,
            });
            Ok((name, params, Box::new(audio_side)))
        }
    }

    fn set_param(&mut self, id: u32, value: f64) {
        if let Some(c) = self.loaded.as_ref().and_then(|l| l.controller.as_ref()) {
            // SAFETY: a plain controller call on the control thread.
            unsafe { c.setParamNormalized(id, value) };
        }
    }

    fn state(&mut self) -> Result<(Vec<u8>, Vec<u8>), String> {
        let l = self.loaded.as_ref().ok_or("not loaded")?;
        // SAFETY: plain calls with streams that outlive them.
        unsafe {
            let component = MemoryStream::writing();
            if l.component.getState(component.as_ptr()) != kResultOk {
                return Err("the plugin gave no state".into());
            }
            let controller = MemoryStream::writing();
            if let Some(c) = &l.controller {
                c.getState(controller.as_ptr());
            }
            Ok((component.take(), controller.take()))
        }
    }
}

/// The name the factory gives class `cid`.
fn class_name(module: &Module, cid: &[u8; 16]) -> Option<String> {
    let f = &module.factory;
    // SAFETY: plain factory calls with a valid out struct.
    unsafe {
        (0..f.countClasses()).find_map(|i| {
            let mut c: PClassInfo = std::mem::zeroed();
            (f.getClassInfo(i, &mut c) == kResultOk && c.cid.map(|b| b as u8) == *cid)
                .then(|| cstr(&c.name))
        })
    }
}

/// The parameters a user may set: not read-only, hidden or program changes.
fn params(c: &ComPtr<IEditController>) -> Vec<Param> {
    use ParameterInfo_::ParameterFlags_::*;
    let skip = kIsReadOnly | kIsHidden | kIsProgramChange;
    // SAFETY: plain controller calls with a valid out struct.
    unsafe {
        (0..c.getParameterCount())
            .filter_map(|i| {
                let mut info: ParameterInfo = std::mem::zeroed();
                if c.getParameterInfo(i, &mut info) != kResultOk || info.flags & skip != 0 {
                    return None;
                }
                Some(Param {
                    id: info.id,
                    title: wstr(&info.title),
                    units: wstr(&info.units),
                    steps: info.stepCount,
                    default: info.defaultNormalizedValue,
                    value: c.getParamNormalized(info.id),
                })
            })
            .collect()
    }
}

/// Set by the plugin through `restartComponent(kLatencyChanged)`.
pub type Flag = Arc<AtomicBool>;

fn take(flag: &Flag) -> bool {
    flag.swap(false, Ordering::Relaxed)
}
