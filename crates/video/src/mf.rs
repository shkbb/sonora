//! The Media Foundation H.264 decoder: coded pictures in, NV12 frames out, turned to BGRA.
//!
//! The samples come from [`crate::fmp4`] already in Annex B form. The decoder holds a few of
//! them back to reorder B-frames, so a push can give back no frame or several.

use std::mem::ManuallyDrop;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_VERSION, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL,
    MFSampleExtension_CleanPoint, MFStartup, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};

use crate::Frame;
use crate::fmp4::Track;

/// Media Foundation counts time in units of 100 nanoseconds.
const TICK: u64 = 100;

/// Decodes one H.264 track into BGRA frames of its display size.
pub struct Decoder {
    mft: IMFTransform,
    /// The picture as shown, which the coded picture may pad below and to the right.
    width: u32,
    height: u32,
    /// The decoded picture's size and the bytes between the starts of two of its rows.
    coded: (u32, u32),
    stride: usize,
    /// Whether the decoder hands out its own output samples, and how big one must be if not.
    provides: bool,
    output_size: u32,
}

impl Decoder {
    /// A decoder for `track`, ready for its first key frame.
    pub fn new(track: &Track) -> Result<Self> {
        start()?;
        // SAFETY: COM is started on this thread, and every interface and type set here
        // outlives the call it is passed to.
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .context("windows has no h.264 decoder")?;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (track.width as u64) << 32 | track.height as u64,
            )?;
            mft.SetInputType(0, &input, 0)
                .context("the decoder refuses the h.264 stream")?;
            let mut decoder = Self {
                mft,
                width: track.width,
                height: track.height,
                coded: (track.width, track.height),
                stride: track.width as usize,
                provides: false,
                output_size: 0,
            };
            decoder.negotiate()?;
            decoder
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            decoder
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(decoder)
        }
    }

    /// Feeds one coded picture shown at `at` and gives back every frame it let out.
    pub fn push(
        &mut self,
        nal: &[u8],
        at: Duration,
        length: Duration,
        key: bool,
    ) -> Result<Vec<Frame>> {
        let sample = coded_sample(nal, at, length, key)?;
        let mut frames = Vec::new();
        loop {
            // SAFETY: the decoder and the sample are live for the call.
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                Err(error) if error.code() == MF_E_NOTACCEPTING => {
                    let before = frames.len();
                    self.pull(&mut frames)?;
                    if frames.len() == before {
                        bail!("the decoder takes no input and gives no output");
                    }
                }
                Err(error) => return Err(error).context("the decoder refused a picture"),
            }
        }
        self.pull(&mut frames)?;
        Ok(frames)
    }

    /// Drops every picture the decoder holds, for a seek. The next push must be a key frame.
    pub fn flush(&mut self) -> Result<()> {
        // SAFETY: the decoder is live.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }
            .context("cannot flush the decoder")
    }

    /// The frames still held back at the end of the stream.
    pub fn drain(&mut self) -> Result<Vec<Frame>> {
        // SAFETY: the decoder is live.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }?;
        let mut frames = Vec::new();
        self.pull(&mut frames)?;
        Ok(frames)
    }

    /// Takes every frame the decoder has ready.
    fn pull(&mut self, frames: &mut Vec<Frame>) -> Result<()> {
        loop {
            let sample = match self.provides {
                true => None,
                false => Some(output_sample(self.output_size)?),
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            // SAFETY: the buffer array lives for the call, and what the decoder put in it is
            // taken back out and dropped below.
            let result = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };
            let [buffer] = buffers;
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => {
                    let sample = sample.context("the decoder gave an empty frame")?;
                    frames.push(self.frame(&sample)?);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.negotiate()?,
                Err(error) => return Err(error).context("the decoder failed a frame"),
            }
        }
    }

    /// Picks NV12 out of what the decoder offers and reads the size its frames come in.
    fn negotiate(&mut self) -> Result<()> {
        // SAFETY: the decoder is live and the types it returns are owned here.
        unsafe {
            let mut index = 0;
            let chosen: IMFMediaType = loop {
                let offered = self
                    .mft
                    .GetOutputAvailableType(0, index)
                    .context("the decoder offers no nv12 output")?;
                if offered.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    break offered;
                }
                index += 1;
            };
            self.mft
                .SetOutputType(0, &chosen, 0)
                .context("the decoder refuses nv12 output")?;
            let size = chosen.GetUINT64(&MF_MT_FRAME_SIZE)?;
            self.coded = ((size >> 32) as u32, size as u32);
            self.stride = match chosen.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(stride) if stride as i32 > 0 => stride as usize,
                _ => self.coded.0 as usize,
            };
            let info = self.mft.GetOutputStreamInfo(0)?;
            self.provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.output_size = info.cbSize;
        }
        log::debug!(
            "video: decoding {}x{} into {}x{} nv12, stride {}",
            self.width,
            self.height,
            self.coded.0,
            self.coded.1,
            self.stride
        );
        Ok(())
    }

    /// A decoded sample as a BGRA frame of the display size.
    fn frame(&self, sample: &IMFSample) -> Result<Frame> {
        // SAFETY: the buffer stays locked while its bytes are read and is unlocked after.
        unsafe {
            let at = sample.GetSampleTime().unwrap_or(0).max(0) as u64 * TICK;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let nv12 = std::slice::from_raw_parts(data, length as usize);
            let bgra = nv12_to_bgra(
                nv12,
                self.stride,
                self.coded.1 as usize,
                self.width,
                self.height,
            );
            buffer.Unlock()?;
            Ok(Frame {
                at: Duration::from_nanos(at),
                width: self.width,
                height: self.height,
                bgra: bgra?,
            })
        }
    }
}

/// A Media Foundation sample holding one coded picture.
fn coded_sample(nal: &[u8], at: Duration, length: Duration, key: bool) -> Result<IMFSample> {
    // SAFETY: the buffer is locked while the bytes are copied in and unlocked after.
    unsafe {
        let buffer = MFCreateMemoryBuffer(nal.len() as u32)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        std::ptr::copy_nonoverlapping(nal.as_ptr(), data, nal.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(nal.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime((at.as_nanos() as u64 / TICK) as i64)?;
        sample.SetSampleDuration((length.as_nanos() as u64 / TICK) as i64)?;
        if key {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        }
        Ok(sample)
    }
}

/// An empty sample for a decoder that wants its output written into one of the caller's.
fn output_sample(size: u32) -> Result<IMFSample> {
    // SAFETY: plain creation calls.
    unsafe {
        let buffer = MFCreateMemoryBuffer(size)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

/// Converts an NV12 picture with `rows` luma rows of `stride` bytes into BGRA of the display
/// size, with the BT.709 limited-range matrix HD video is coded in.
fn nv12_to_bgra(
    nv12: &[u8],
    stride: usize,
    rows: usize,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let (width, height) = (width as usize, height as usize);
    let chroma = stride * rows;
    if nv12.len() < chroma + stride * rows.div_ceil(2) || width > stride || height > rows {
        bail!("an nv12 frame of {} bytes is too small", nv12.len());
    }
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let luma = &nv12[y * stride..y * stride + width];
        let uv = &nv12[chroma + (y / 2) * stride..];
        let row = &mut out[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let c = (luma[x] as i32 - 16) * 298;
            let d = uv[x & !1] as i32 - 128;
            let e = uv[(x & !1) + 1] as i32 - 128;
            let pixel = &mut row[x * 4..x * 4 + 4];
            pixel[0] = clamp((c + 541 * d + 128) >> 8);
            pixel[1] = clamp((c - 55 * d - 137 * e + 128) >> 8);
            pixel[2] = clamp((c + 459 * e + 128) >> 8);
            pixel[3] = 255;
        }
    }
    Ok(out)
}

fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Starts COM and Media Foundation for this thread. Both count their starts, so calling it
/// again is harmless.
fn start() -> Result<()> {
    // SAFETY: plain initialisation calls with no pointers.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("cannot start com")?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("cannot start media foundation")
    }
}
