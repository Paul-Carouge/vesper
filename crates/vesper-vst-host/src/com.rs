//! The COM objects a VST3 host hands to plugins, and the audio side.

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void};

use vesper_vst::host::Processor;
use vesper_vst::protocol::Load;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};

use crate::{Flag, take};

/// A NUL-terminated `char8` field.
pub fn cstr(s: &[c_char]) -> String {
    let bytes: Vec<u8> = s
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A NUL-terminated UTF-16 field.
pub fn wstr(s: &[TChar]) -> String {
    let units: Vec<u16> = s.iter().take_while(|&&c| c != 0).copied().collect();
    String::from_utf16_lossy(&units)
}

/// The host context and component handler given to the plugin.
#[derive(Default)]
pub struct HostApp {
    pub latency_changed: Flag,
}

impl Class for HostApp {
    type Interfaces = (IHostApplication, IComponentHandler);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        // SAFETY: the plugin passes a String128.
        let name = unsafe { &mut *name };
        let text: Vec<u16> = "Vesper".encode_utf16().chain([0]).collect();
        for (d, s) in name.iter_mut().zip(text) {
            *d = s as TChar;
        }
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        _cid: *mut TUID,
        _iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        // shortcut: no IMessage/IAttributeList; plugins that need them for
        // component-controller messages fall back or lose that feature.
        if !obj.is_null() {
            // SAFETY: a valid out pointer.
            unsafe { *obj = std::ptr::null_mut() };
        }
        kResultFalse
    }
}

impl IComponentHandlerTrait for HostApp {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, _id: ParamID, _value: ParamValue) -> tresult {
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        if flags & RestartFlags_::kLatencyChanged as int32 != 0 {
            self.latency_changed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        kResultOk
    }
}

/// An in-memory IBStream, for states.
pub struct MemoryStream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

/// A stream and the interface pointer handed to plugins.
pub struct Stream(ComWrapper<MemoryStream>, ComPtr<IBStream>);

impl MemoryStream {
    pub fn reading(data: &[u8]) -> Stream {
        let s = ComWrapper::new(MemoryStream {
            data: RefCell::new(data.to_vec()),
            pos: Cell::new(0),
        });
        let p = s.to_com_ptr::<IBStream>().expect("IBStream");
        Stream(s, p)
    }

    pub fn writing() -> Stream {
        Self::reading(&[])
    }
}

impl Stream {
    pub fn as_ptr(&self) -> *mut IBStream {
        self.1.as_ptr()
    }

    pub fn take(&self) -> Vec<u8> {
        self.0.data.take()
    }
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, n: int32, read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (n.max(0) as usize).min(data.len() - pos);
        // SAFETY: the plugin's buffer holds at least the `n` it asked for.
        unsafe { std::ptr::copy_nonoverlapping(data[pos..].as_ptr(), buffer.cast(), n) };
        self.pos.set(pos + n);
        if !read.is_null() {
            // SAFETY: a valid out pointer.
            unsafe { *read = n as int32 };
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, n: int32, written: *mut int32) -> tresult {
        let n = n.max(0) as usize;
        // SAFETY: the plugin's buffer holds `n` bytes.
        let src = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
        data[pos..pos + n].copy_from_slice(src);
        self.pos.set(pos + n);
        if !written.is_null() {
            // SAFETY: a valid out pointer.
            unsafe { *written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let base = match mode as IBStream_::IStreamSeekMode {
            IBStream_::IStreamSeekMode_::kIBSeekSet => 0,
            IBStream_::IStreamSeekMode_::kIBSeekCur => self.pos.get() as int64,
            IBStream_::IStreamSeekMode_::kIBSeekEnd => self.data.borrow().len() as int64,
            _ => return kInvalidArgument,
        };
        let new = base + pos;
        if new < 0 {
            return kInvalidArgument;
        }
        self.pos.set(new as usize);
        if !result.is_null() {
            // SAFETY: a valid out pointer.
            unsafe { *result = new };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a valid out pointer.
        unsafe { *pos = self.pos.get() as int64 };
        kResultOk
    }
}

/// One parameter's changes in a block: at most one point, at offset 0.
struct ParamQueue {
    id: Cell<ParamID>,
    value: Cell<f64>,
    points: Cell<int32>,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.points.get()
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 || self.points.get() == 0 || offset.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: valid out pointers.
        unsafe { (*offset, *value) = (0, self.value.get()) };
        kResultOk
    }

    unsafe fn addPoint(&self, _offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        self.value.set(value);
        self.points.set(1);
        if !index.is_null() {
            // SAFETY: a valid out pointer.
            unsafe { *index = 0 };
        }
        kResultOk
    }
}

/// Parameter changes of a block, from a fixed pool (no allocation).
pub struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    ptrs: Vec<ComPtr<IParamValueQueue>>,
    used: Cell<usize>,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl ParamChanges {
    fn new(capacity: usize) -> Self {
        let queues: Vec<_> = (0..capacity)
            .map(|_| {
                ComWrapper::new(ParamQueue {
                    id: Cell::new(0),
                    value: Cell::new(0.0),
                    points: Cell::new(0),
                })
            })
            .collect();
        let ptrs = queues
            .iter()
            .map(|q| q.to_com_ptr::<IParamValueQueue>().expect("queue"))
            .collect();
        Self {
            queues,
            ptrs,
            used: Cell::new(0),
        }
    }

    /// The queue of `id`, added if new. None when the pool is full.
    fn queue(&self, id: ParamID) -> Option<usize> {
        let used = self.used.get();
        if let Some(i) = (0..used).find(|&i| self.queues[i].id.get() == id) {
            return Some(i);
        }
        let q = self.queues.get(used)?;
        q.id.set(id);
        q.points.set(0);
        self.used.set(used + 1);
        Some(used)
    }

    fn set(&self, changes: &[(u32, f64)]) {
        self.used.set(0);
        for &(id, value) in changes {
            if let Some(i) = self.queue(id) {
                self.queues[i].value.set(value);
                self.queues[i].points.set(1);
            }
        }
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        match usize::try_from(index) {
            Ok(i) if i < self.used.get() => self.ptrs[i].as_ptr(),
            _ => std::ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut int32,
    ) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: a valid pointer from the caller.
        let Some(i) = self.queue(unsafe { *id }) else {
            return std::ptr::null_mut();
        };
        if !index.is_null() {
            // SAFETY: a valid out pointer.
            unsafe { *index = i as int32 };
        }
        self.ptrs[i].as_ptr()
    }
}

/// One bus's planar channel buffers.
struct Bus {
    data: Vec<Vec<f32>>,
    ptrs: Vec<*mut f32>,
}

impl Bus {
    fn new(channels: usize, frames: usize) -> Self {
        let mut data: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0; frames]).collect();
        let ptrs = data.iter_mut().map(|c| c.as_mut_ptr()).collect();
        Self { data, ptrs }
    }
}

/// The plugin's audio side, on the host's audio thread.
pub struct Audio {
    processor: ComPtr<IAudioProcessor>,
    latency_changed: Flag,
    latency: u32,
    changes: ComWrapper<ParamChanges>,
    changes_ptr: ComPtr<IParameterChanges>,
    inputs: Vec<Bus>,
    outputs: Vec<Bus>,
    in_buffers: Vec<AudioBusBuffers>,
    out_buffers: Vec<AudioBusBuffers>,
    context: ProcessContext,
}

// SAFETY: VST3 processors are made to run on an audio thread other than
// the one that created them; this object is used by one thread at a time.
unsafe impl Send for Audio {}

/// Most parameters changed in one block.
const MAX_CHANGES: usize = 256;

fn buffers(buses: &mut [Bus]) -> Vec<AudioBusBuffers> {
    buses
        .iter_mut()
        .map(|b| AudioBusBuffers {
            numChannels: b.ptrs.len() as int32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: b.ptrs.as_mut_ptr(),
            },
        })
        .collect()
}

impl Audio {
    pub fn new(
        processor: ComPtr<IAudioProcessor>,
        latency_changed: Flag,
        load: &Load,
        ins: &[usize],
        outs: &[usize],
    ) -> Self {
        let frames = load.max_frames as usize;
        let mut inputs: Vec<Bus> = ins.iter().map(|&c| Bus::new(c, frames)).collect();
        let mut outputs: Vec<Bus> = outs.iter().map(|&c| Bus::new(c, frames)).collect();
        let changes = ComWrapper::new(ParamChanges::new(MAX_CHANGES));
        let changes_ptr = changes.to_com_ptr::<IParameterChanges>().expect("changes");
        // SAFETY: zero is valid for this plain C struct.
        let mut context: ProcessContext = unsafe { std::mem::zeroed() };
        use ProcessContext_::StatesAndFlags_::*;
        context.state = kPlaying | kTempoValid | kTimeSigValid | kContTimeValid;
        context.sampleRate = f64::from(load.sample_rate);
        context.tempo = 120.0;
        (context.timeSigNumerator, context.timeSigDenominator) = (4, 4);
        // SAFETY: a plain call.
        let latency = unsafe { processor.getLatencySamples() };
        Self {
            in_buffers: buffers(&mut inputs),
            out_buffers: buffers(&mut outputs),
            processor,
            latency_changed,
            latency,
            changes,
            changes_ptr,
            inputs,
            outputs,
            context,
        }
    }
}

impl Processor for Audio {
    fn process(&mut self, input: &[f32], output: &mut [f32], changes: &[(u32, f64)]) {
        // Vesper's side is stereo (checked at load).
        let frames = input.len() / 2;
        let main = &mut self.inputs[0].data;
        for (i, f) in input.as_chunks::<2>().0.iter().enumerate() {
            if main.len() == 2 {
                (main[0][i], main[1][i]) = (f[0], f[1]);
            } else {
                main[0][i] = 0.5 * (f[0] + f[1]);
            }
        }
        for (b, buf) in self.inputs.iter().zip(&mut self.in_buffers).skip(1) {
            // Side-chains and other extra inputs: silent.
            buf.silenceFlags = (1u64 << b.data.len()) - 1;
        }
        self.changes.set(changes);
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            numSamples: frames as int32,
            numInputs: self.in_buffers.len() as int32,
            numOutputs: self.out_buffers.len() as int32,
            inputs: self.in_buffers.as_mut_ptr(),
            outputs: self.out_buffers.as_mut_ptr(),
            inputParameterChanges: self.changes_ptr.as_ptr(),
            outputParameterChanges: std::ptr::null_mut(),
            inputEvents: std::ptr::null_mut(),
            outputEvents: std::ptr::null_mut(),
            processContext: &mut self.context,
        };
        // SAFETY: every buffer holds max_frames >= frames samples.
        let ok = unsafe { self.processor.process(&mut data) } == kResultOk;
        let out = &self.outputs[0].data;
        for (i, f) in output.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            (f[0], f[1]) = match (ok, out.len()) {
                (false, _) => (input[2 * i], input[2 * i + 1]),
                (true, 2) => (out[0][i], out[1][i]),
                (true, _) => (out[0][i], out[0][i]),
            };
        }
        self.context.projectTimeSamples += frames as i64;
        self.context.continousTimeSamples += frames as i64;
        if take(&self.latency_changed) {
            // SAFETY: a plain call.
            self.latency = unsafe { self.processor.getLatencySamples() };
        }
    }

    fn latency(&self) -> u32 {
        self.latency
    }
}
