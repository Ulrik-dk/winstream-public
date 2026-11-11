#[cfg(feature = "net")]
pub mod net;
#[cfg(feature = "opus")]
pub mod opus;

pub const ALPN: &[u8] = b"winstream/2";
pub const DEFAULT_PORT: u16 = 4433;
pub const FEEDBACK_INTERVAL_MS: u64 = 100;
pub const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(2);

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;
pub const AUDIO_FRAME_MS: u32 = 10;
pub const AUDIO_FRAME_SAMPLES: usize = (AUDIO_RATE * AUDIO_FRAME_MS / 1000) as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub seq: u32,
    pub capture_ns: u64,
}

impl AudioHeader {
    pub const LEN: usize = 12;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..4].copy_from_slice(&self.seq.to_le_bytes());
        b[4..].copy_from_slice(&self.capture_ns.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        Some(AudioHeader {
            seq: u32::from_le_bytes(b.get(..4)?.try_into().ok()?),
            capture_ns: u64::from_le_bytes(b.get(4..12)?.try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Ping {
        client_ns: u64,
    },
    Keymap {
        rules: String,
        model: String,
        layout: String,
        variant: String,
        options: String,
    },
    Key {
        code: u32,
        pressed: bool,
    },
    PointerMotion {
        window: u32,
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    Scroll {
        dx: f64,
        dy: f64,
        steps_x: i32,
        steps_y: i32,
    },
    Focus {
        window: u32,
    },
    Resize {
        window: u32,
        width: u32,
        height: u32,
    },
    Close {
        window: u32,
    },
    Display {
        width: u32,
        height: u32,
        refresh_mhz: u32,
    },
    RelativeMotion {
        dx: f64,
        dy: f64,
    },
    Feedback {
        window: u32,
        received: u32,
        shown: u32,
        last_frame_id: u64,
        net_delay_us: u32,
        latency_us: u32,
    },

    Pong {
        client_ns: u64,
        server_ns: u64,
    },
    WindowOpened {
        window: u32,
        parent: u32,
        width: u32,
        height: u32,
        title: String,
        app_id: String,
    },
    WindowTitle {
        window: u32,
        title: String,
        app_id: String,
    },
    WindowClosed {
        window: u32,
    },
    PointerGrab {
        window: u32,
        mode: PointerGrab,
    },
    Fullscreen {
        window: u32,
        on: bool,
    },
    Selection {
        primary: bool,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerGrab {
    None,
    Confined,
    Locked,
}

struct Writer(Vec<u8>);
impl Writer {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f64(&mut self, v: f64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bool(&mut self, v: bool) -> &mut Self {
        self.0.push(v as u8);
        self
    }
    fn str(&mut self, v: &str) -> &mut Self {
        let bytes = &v.as_bytes()[..v.len().min(u16::MAX as usize)];
        self.0
            .extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        self.0.extend_from_slice(bytes);
        self
    }
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bool(&mut self) -> Option<bool> {
        Some(self.take(1)?[0] != 0)
    }
    fn str(&mut self) -> Option<String> {
        let len = u16::from_le_bytes(self.take(2)?.try_into().ok()?) as usize;
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
}

impl Control {
    pub const HEADER_LEN: usize = 3;
    pub const MAX_SELECTION: usize = 60_000;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        let tag = match self {
            Control::Ping { client_ns } => {
                w.u64(*client_ns);
                1
            }
            Control::Pong {
                client_ns,
                server_ns,
            } => {
                w.u64(*client_ns).u64(*server_ns);
                2
            }
            Control::Keymap {
                rules,
                model,
                layout,
                variant,
                options,
            } => {
                w.str(rules)
                    .str(model)
                    .str(layout)
                    .str(variant)
                    .str(options);
                3
            }
            Control::Key { code, pressed } => {
                w.u32(*code).bool(*pressed);
                4
            }
            Control::PointerMotion { window, x, y } => {
                w.u32(*window).f64(*x).f64(*y);
                5
            }
            Control::PointerButton { button, pressed } => {
                w.u32(*button).bool(*pressed);
                6
            }
            Control::Scroll {
                dx,
                dy,
                steps_x,
                steps_y,
            } => {
                w.f64(*dx).f64(*dy).i32(*steps_x).i32(*steps_y);
                7
            }
            Control::Focus { window } => {
                w.u32(*window);
                8
            }
            Control::Resize {
                window,
                width,
                height,
            } => {
                w.u32(*window).u32(*width).u32(*height);
                9
            }
            Control::Close { window } => {
                w.u32(*window);
                10
            }
            Control::WindowOpened {
                window,
                parent,
                width,
                height,
                title,
                app_id,
            } => {
                w.u32(*window)
                    .u32(*parent)
                    .u32(*width)
                    .u32(*height)
                    .str(title)
                    .str(app_id);
                11
            }
            Control::WindowTitle {
                window,
                title,
                app_id,
            } => {
                w.u32(*window).str(title).str(app_id);
                12
            }
            Control::WindowClosed { window } => {
                w.u32(*window);
                13
            }
            Control::Display {
                width,
                height,
                refresh_mhz,
            } => {
                w.u32(*width).u32(*height).u32(*refresh_mhz);
                14
            }
            Control::RelativeMotion { dx, dy } => {
                w.f64(*dx).f64(*dy);
                15
            }
            Control::PointerGrab { window, mode } => {
                let m = match mode {
                    PointerGrab::None => 0,
                    PointerGrab::Confined => 1,
                    PointerGrab::Locked => 2,
                };
                w.u32(*window).u32(m);
                16
            }
            Control::Fullscreen { window, on } => {
                w.u32(*window).bool(*on);
                17
            }
            Control::Selection { primary, text } => {
                w.bool(*primary).str(text);
                18
            }
            Control::Feedback {
                window,
                received,
                shown,
                last_frame_id,
                net_delay_us,
                latency_us,
            } => {
                w.u32(*window)
                    .u32(*received)
                    .u32(*shown)
                    .u64(*last_frame_id)
                    .u32(*net_delay_us)
                    .u32(*latency_us);
                19
            }
        };
        let payload = w.0;
        let mut out = Vec::with_capacity(Self::HEADER_LEN + payload.len());
        out.push(tag);
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    pub fn payload_len(header: &[u8; Self::HEADER_LEN]) -> usize {
        u16::from_le_bytes([header[1], header[2]]) as usize
    }

    pub fn decode(header: &[u8; Self::HEADER_LEN], payload: &[u8]) -> Option<Self> {
        let mut r = Reader(payload);
        Some(match header[0] {
            1 => Control::Ping {
                client_ns: r.u64()?,
            },
            2 => Control::Pong {
                client_ns: r.u64()?,
                server_ns: r.u64()?,
            },
            3 => Control::Keymap {
                rules: r.str()?,
                model: r.str()?,
                layout: r.str()?,
                variant: r.str()?,
                options: r.str()?,
            },
            4 => Control::Key {
                code: r.u32()?,
                pressed: r.bool()?,
            },
            5 => Control::PointerMotion {
                window: r.u32()?,
                x: r.f64()?,
                y: r.f64()?,
            },
            6 => Control::PointerButton {
                button: r.u32()?,
                pressed: r.bool()?,
            },
            7 => Control::Scroll {
                dx: r.f64()?,
                dy: r.f64()?,
                steps_x: r.i32()?,
                steps_y: r.i32()?,
            },
            8 => Control::Focus { window: r.u32()? },
            9 => Control::Resize {
                window: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
            },
            10 => Control::Close { window: r.u32()? },
            11 => Control::WindowOpened {
                window: r.u32()?,
                parent: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                title: r.str()?,
                app_id: r.str()?,
            },
            12 => Control::WindowTitle {
                window: r.u32()?,
                title: r.str()?,
                app_id: r.str()?,
            },
            13 => Control::WindowClosed { window: r.u32()? },
            14 => Control::Display {
                width: r.u32()?,
                height: r.u32()?,
                refresh_mhz: r.u32()?,
            },
            15 => Control::RelativeMotion {
                dx: r.f64()?,
                dy: r.f64()?,
            },
            16 => Control::PointerGrab {
                window: r.u32()?,
                mode: match r.u32()? {
                    0 => PointerGrab::None,
                    1 => PointerGrab::Confined,
                    2 => PointerGrab::Locked,
                    _ => return None,
                },
            },
            17 => Control::Fullscreen {
                window: r.u32()?,
                on: r.bool()?,
            },
            18 => Control::Selection {
                primary: r.bool()?,
                text: r.str()?,
            },
            19 => Control::Feedback {
                window: r.u32()?,
                received: r.u32()?,
                shown: r.u32()?,
                last_frame_id: r.u64()?,
                net_delay_us: r.u32()?,
                latency_us: r.u32()?,
            },
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub window_id: u32,
    pub frame_id: u64,
    pub commit_ns: u64,
    pub encoded_ns: u64,
    pub width: u16,
    pub height: u16,
    pub visible_width: u16,
    pub visible_height: u16,
    pub keyframe: bool,
    pub len: u32,
}

impl FrameHeader {
    pub const LEN: usize = 41;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut w = Writer(Vec::with_capacity(Self::LEN));
        w.u32(self.window_id)
            .u64(self.frame_id)
            .u64(self.commit_ns)
            .u64(self.encoded_ns);
        for v in [
            self.width,
            self.height,
            self.visible_width,
            self.visible_height,
        ] {
            w.0.extend_from_slice(&v.to_le_bytes());
        }
        w.bool(self.keyframe).u32(self.len);
        w.0.try_into().expect("header length")
    }

    pub fn decode(b: &[u8; Self::LEN]) -> Self {
        let mut r = Reader(b);
        let u16 = |r: &mut Reader| u16::from_le_bytes(r.take(2).unwrap().try_into().unwrap());
        let window_id = r.u32().unwrap();
        let frame_id = r.u64().unwrap();
        let commit_ns = r.u64().unwrap();
        let encoded_ns = r.u64().unwrap();
        FrameHeader {
            window_id,
            frame_id,
            commit_ns,
            encoded_ns,
            width: u16(&mut r),
            height: u16(&mut r),
            visible_width: u16(&mut r),
            visible_height: u16(&mut r),
            keyframe: r.bool().unwrap(),
            len: r.u32().unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip() {
        for m in [
            Control::Ping { client_ns: 42 },
            Control::Pong {
                client_ns: 1,
                server_ns: u64::MAX,
            },
            Control::Keymap {
                rules: "evdev".into(),
                model: "pc105".into(),
                layout: "dk".into(),
                variant: "".into(),
                options: "".into(),
            },
            Control::Key {
                code: 30,
                pressed: true,
            },
            Control::PointerMotion {
                window: 2,
                x: 10.5,
                y: -3.0,
            },
            Control::PointerButton {
                button: 0x110,
                pressed: false,
            },
            Control::Scroll {
                dx: 0.0,
                dy: 15.0,
                steps_x: 0,
                steps_y: 120,
            },
            Control::Focus { window: 1 },
            Control::Resize {
                window: 1,
                width: 800,
                height: 600,
            },
            Control::Close { window: 3 },
            Control::WindowOpened {
                window: 1,
                parent: 0,
                width: 640,
                height: 480,
                title: "Steam – æøå".into(),
                app_id: "steam".into(),
            },
            Control::WindowTitle {
                window: 1,
                title: "x".into(),
                app_id: "".into(),
            },
            Control::WindowClosed { window: 1 },
            Control::Display {
                width: 2560,
                height: 1440,
                refresh_mhz: 59_950,
            },
            Control::RelativeMotion { dx: -3.5, dy: 2.0 },
            Control::PointerGrab {
                window: 2,
                mode: PointerGrab::Locked,
            },
            Control::Fullscreen {
                window: 2,
                on: true,
            },
            Control::Selection {
                primary: true,
                text: "valgt tekst – æøå\nlinje 2".into(),
            },
            Control::Feedback {
                window: 2,
                received: 15,
                shown: 14,
                last_frame_id: 9_000,
                net_delay_us: 6_100,
                latency_us: 9_400,
            },
        ] {
            let bytes = m.encode();
            let header: [u8; Control::HEADER_LEN] = bytes[..3].try_into().unwrap();
            assert_eq!(Control::payload_len(&header), bytes.len() - 3);
            assert_eq!(Control::decode(&header, &bytes[3..]), Some(m));
        }
    }

    #[test]
    fn audio_header_roundtrip() {
        let h = AudioHeader {
            seq: 9,
            capture_ns: 123_456,
        };
        assert_eq!(AudioHeader::decode(&h.encode()), Some(h));
        assert_eq!(AUDIO_FRAME_SAMPLES, 480);
    }

    #[test]
    fn header_roundtrip() {
        let h = FrameHeader {
            window_id: 3,
            frame_id: 7,
            commit_ns: 123,
            encoded_ns: 456,
            width: 1280,
            height: 720,
            visible_width: 1279,
            visible_height: 719,
            keyframe: true,
            len: 35_000,
        };
        assert_eq!(FrameHeader::decode(&h.encode()), h);
    }
}
