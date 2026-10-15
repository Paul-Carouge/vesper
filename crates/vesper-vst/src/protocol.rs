//! Control messages between Vesper and a plugin host, over the host's Unix
//! socket: a little-endian `u32` body length, then the body (a tag byte and
//! the fields). Audio does not go through here but through [`crate::shm`].

use std::io::{self, Read, Write};

/// Largest message body: plugin states can be big, not this big.
pub const MAX_MESSAGE: usize = 64 << 20;

/// What the host is asked to load.
#[derive(Debug, Clone, PartialEq)]
pub struct Load {
    /// The `.vst3` bundle (or module file), as bytes: paths need not be UTF-8.
    pub path: Vec<u8>,
    /// Class id of the audio processor component.
    pub cid: [u8; 16],
    pub channels: u32,
    /// Most frames in one block.
    pub max_frames: u32,
    pub sample_rate: u32,
    /// States to restore (empty: the plugin's defaults).
    pub component: Vec<u8>,
    pub controller: Vec<u8>,
}

/// A plugin parameter, for generic sliders.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub id: u32,
    pub title: String,
    pub units: String,
    /// 0: continuous; n: n + 1 discrete values.
    pub steps: i32,
    /// Normalized, 0..=1.
    pub default: f64,
    /// Normalized, 0..=1.
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// Vesper to host: load the plugin, map the audio region (fd 4) and
    /// start serving blocks. Answered by `Loaded` or `Error`.
    Load(Load),
    /// Vesper to host: set a parameter (normalized). Not answered.
    SetParam {
        id: u32,
        value: f64,
    },
    /// Vesper to host: answered by `State` or `Error`.
    GetState,
    Loaded {
        name: String,
        latency: u32,
        params: Vec<Param>,
    },
    State {
        component: Vec<u8>,
        controller: Vec<u8>,
    },
    Error(String),
}

const LOAD: u8 = 1;
const SET_PARAM: u8 = 2;
const GET_STATE: u8 = 3;
const LOADED: u8 = 10;
const STATE: u8 = 11;
const ERROR: u8 = 12;

struct Writer(Vec<u8>);

impl Writer {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }
    fn string(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?).ok()
    }
}

impl Msg {
    /// The body, without the length.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        match self {
            Msg::Load(l) => {
                w.0.push(LOAD);
                w.bytes(&l.path);
                w.0.extend_from_slice(&l.cid);
                w.u32(l.channels);
                w.u32(l.max_frames);
                w.u32(l.sample_rate);
                w.bytes(&l.component);
                w.bytes(&l.controller);
            }
            Msg::SetParam { id, value } => {
                w.0.push(SET_PARAM);
                w.u32(*id);
                w.f64(*value);
            }
            Msg::GetState => w.0.push(GET_STATE),
            Msg::Loaded {
                name,
                latency,
                params,
            } => {
                w.0.push(LOADED);
                w.bytes(name.as_bytes());
                w.u32(*latency);
                w.u32(params.len() as u32);
                for p in params {
                    w.u32(p.id);
                    w.bytes(p.title.as_bytes());
                    w.bytes(p.units.as_bytes());
                    w.u32(p.steps as u32);
                    w.f64(p.default);
                    w.f64(p.value);
                }
            }
            Msg::State {
                component,
                controller,
            } => {
                w.0.push(STATE);
                w.bytes(component);
                w.bytes(controller);
            }
            Msg::Error(message) => {
                w.0.push(ERROR);
                w.bytes(message.as_bytes());
            }
        }
        w.0
    }

    /// Parses a body. None if it is not exactly one well-formed message.
    pub fn decode(body: &[u8]) -> Option<Msg> {
        let mut r = Reader(body);
        let msg = match r.u8()? {
            LOAD => Msg::Load(Load {
                path: r.bytes()?,
                cid: r.take(16)?.try_into().ok()?,
                channels: r.u32()?,
                max_frames: r.u32()?,
                sample_rate: r.u32()?,
                component: r.bytes()?,
                controller: r.bytes()?,
            }),
            SET_PARAM => Msg::SetParam {
                id: r.u32()?,
                value: r.f64()?,
            },
            GET_STATE => Msg::GetState,
            LOADED => {
                let name = r.string()?;
                let latency = r.u32()?;
                let count = r.u32()?;
                // No capacity from the count: it is not trusted.
                let mut params = Vec::new();
                for _ in 0..count {
                    params.push(Param {
                        id: r.u32()?,
                        title: r.string()?,
                        units: r.string()?,
                        steps: r.u32()? as i32,
                        default: r.f64()?,
                        value: r.f64()?,
                    });
                }
                Msg::Loaded {
                    name,
                    latency,
                    params,
                }
            }
            STATE => Msg::State {
                component: r.bytes()?,
                controller: r.bytes()?,
            },
            ERROR => Msg::Error(r.string()?),
            _ => return None,
        };
        r.0.is_empty().then_some(msg)
    }
}

/// Sends one message.
pub fn write(w: &mut impl Write, msg: &Msg) -> io::Result<()> {
    let body = msg.encode();
    if body.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message too big",
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)
}

/// Receives one message. `UnexpectedEof` when the peer closed the socket,
/// `InvalidData` on a malformed one.
pub fn read(r: &mut impl Read) -> io::Result<Msg> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    let bad = |what| io::Error::new(io::ErrorKind::InvalidData, what);
    if len > MAX_MESSAGE {
        return Err(bad("message too big"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Msg::decode(&body).ok_or_else(|| bad("malformed message"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_message() -> Vec<Msg> {
        vec![
            Msg::Load(Load {
                path: b"/usr/lib/vst3/\xffodd.vst3".to_vec(),
                cid: *b"0123456789abcdef",
                channels: 2,
                max_frames: 1024,
                sample_rate: 48_000,
                component: vec![1, 2, 3],
                controller: Vec::new(),
            }),
            Msg::SetParam { id: 7, value: 0.25 },
            Msg::GetState,
            Msg::Loaded {
                name: "Gain ü".into(),
                latency: 64,
                params: vec![
                    Param {
                        id: 0,
                        title: "Gain".into(),
                        units: "dB".into(),
                        steps: 0,
                        default: 0.5,
                        value: 0.75,
                    },
                    Param {
                        id: 9,
                        title: "Mode".into(),
                        units: String::new(),
                        steps: 3,
                        default: 0.0,
                        value: 1.0,
                    },
                ],
            },
            Msg::State {
                component: vec![0; 70_000],
                controller: vec![9],
            },
            Msg::Error("no stereo bus".into()),
        ]
    }

    #[test]
    fn messages_round_trip() {
        let mut stream = Vec::new();
        for msg in every_message() {
            assert_eq!(Msg::decode(&msg.encode()), Some(msg.clone()));
            write(&mut stream, &msg).unwrap();
        }
        let mut r = stream.as_slice();
        for msg in every_message() {
            assert_eq!(read(&mut r).unwrap(), msg);
        }
        assert_eq!(
            read(&mut r).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn malformed_messages_are_rejected() {
        for msg in every_message() {
            let body = msg.encode();
            // Every truncation, and a trailing byte.
            for n in 0..body.len() {
                assert_eq!(Msg::decode(&body[..n]), None, "{msg:?} cut at {n}");
            }
            let mut long = body.clone();
            long.push(0);
            assert_eq!(Msg::decode(&long), None);
        }
        assert_eq!(Msg::decode(&[99]), None);
        // A huge parameter count with nothing behind it.
        let mut body = vec![LOADED];
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Msg::decode(&body), None);
        // Invalid UTF-8 in a string.
        let mut body = vec![ERROR];
        body.extend_from_slice(&1u32.to_le_bytes());
        body.push(0xff);
        assert_eq!(Msg::decode(&body), None);
        // A length over the limit is refused before reading the body.
        let mut r = &((MAX_MESSAGE + 1) as u32).to_le_bytes()[..];
        assert_eq!(read(&mut r).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
