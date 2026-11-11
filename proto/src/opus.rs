use std::{fmt, ptr::NonNull};

use opusic_sys as sys;

use crate::{AUDIO_CHANNELS, AUDIO_RATE};

#[derive(Debug)]
pub struct Error(i32);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "opus error {}", self.0)
    }
}

impl std::error::Error for Error {}

fn check(code: i32) -> Result<i32, Error> {
    if code < 0 { Err(Error(code)) } else { Ok(code) }
}

pub struct Encoder(NonNull<sys::OpusEncoder>);

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(bitrate: i32) -> Result<Self, Error> {
        let mut err = 0;
        let ptr = unsafe {
            sys::opus_encoder_create(
                AUDIO_RATE as i32,
                AUDIO_CHANNELS as i32,
                sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        check(err)?;
        let enc = Encoder(NonNull::new(ptr).ok_or(Error(-1))?);
        check(unsafe {
            sys::opus_encoder_ctl(enc.0.as_ptr(), sys::OPUS_SET_BITRATE_REQUEST, bitrate)
        })?;
        Ok(enc)
    }

    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, Error> {
        let frame = (pcm.len() / AUDIO_CHANNELS) as i32;
        let n = unsafe {
            sys::opus_encode(
                self.0.as_ptr(),
                pcm.as_ptr(),
                frame,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        Ok(check(n)? as usize)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { sys::opus_encoder_destroy(self.0.as_ptr()) }
    }
}

pub struct Decoder(NonNull<sys::OpusDecoder>);

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, Error> {
        let mut err = 0;
        let ptr =
            unsafe { sys::opus_decoder_create(AUDIO_RATE as i32, AUDIO_CHANNELS as i32, &mut err) };
        check(err)?;
        Ok(Decoder(NonNull::new(ptr).ok_or(Error(-1))?))
    }

    pub fn decode(&mut self, packet: Option<&[u8]>, out: &mut [i16]) -> Result<usize, Error> {
        let frame = (out.len() / AUDIO_CHANNELS) as i32;
        let (data, len) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32),
            None => (std::ptr::null(), 0),
        };
        let n = unsafe { sys::opus_decode(self.0.as_ptr(), data, len, out.as_mut_ptr(), frame, 0) };
        Ok(check(n)? as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { sys::opus_decoder_destroy(self.0.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AUDIO_FRAME_SAMPLES;

    #[test]
    fn roundtrip_and_concealment() {
        let mut enc = Encoder::new(128_000).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut pcm = vec![0i16; AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS];
        for (i, s) in pcm.iter_mut().enumerate() {
            *s = ((i as f32 * 0.05).sin() * 8000.0) as i16;
        }
        let mut packet = vec![0u8; 4000];
        let mut out = vec![0i16; pcm.len()];
        for _ in 0..5 {
            let n = enc.encode(&pcm, &mut packet).unwrap();
            assert!(n > 0);
            assert_eq!(
                dec.decode(Some(&packet[..n]), &mut out).unwrap(),
                AUDIO_FRAME_SAMPLES
            );
        }
        assert_eq!(dec.decode(None, &mut out).unwrap(), AUDIO_FRAME_SAMPLES);
    }
}
