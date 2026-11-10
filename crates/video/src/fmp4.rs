//! Just enough of fragmented MP4 to pull H.264 out of a YouTube DASH stream.
//!
//! The file opens with a `moov` that holds no samples, only the codec setup, then repeats
//! `moof` and `mdat` pairs, one per segment, each starting on a key frame. Media Foundation's
//! own MP4 reader sees no samples in such a file, so the boxes are read here and the decoder
//! is fed the samples one by one.

use std::ops::Range;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

/// What the init segment says about the one video track.
#[derive(Clone, Debug)]
pub struct Track {
    /// Ticks per second of every time stamp in the fragments.
    pub timescale: u32,
    pub width: u32,
    pub height: u32,
    /// How many bytes prefix each NAL unit's length in a sample.
    pub nal_length: usize,
    /// The sequence and picture parameter sets, sent before every key frame.
    pub parameters: Vec<Vec<u8>>,
    /// Where the first fragment begins.
    pub fragments_at: usize,
}

/// One coded picture: where its bytes are in the file and when it is shown, in track ticks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub bytes: Range<usize>,
    pub decode: u64,
    pub show: u64,
    pub duration: u32,
    pub key: bool,
}

/// The samples of one `moof` and its `mdat`, and where the next fragment starts.
#[derive(Debug)]
pub struct Fragment {
    pub samples: Vec<Sample>,
    pub next: usize,
}

/// Reads the codec setup from the front of `data`, which holds at least the whole `moov`.
pub fn read_init(data: &[u8]) -> Result<Track> {
    let mut at = 0;
    let mut timescale = None;
    let mut setup = None;
    while let Some((kind, body, end)) = next_box(data, at) {
        match &kind {
            b"moov" => {
                let mdia = path(&data[body.clone()], &[b"trak", b"mdia"])
                    .context("the init segment has no media box")?;
                let mdia = &data[body.start + mdia.start..body.start + mdia.end];
                timescale = Some(mdhd_timescale(mdia)?);
                let stsd = path(mdia, &[b"minf", b"stbl", b"stsd"])
                    .context("the init segment has no sample description")?;
                setup = Some(avc1(&mdia[stsd])?);
            }
            b"moof" | b"sidx" if setup.is_some() => {
                let (width, height, nal_length, parameters) = setup.context("no setup")?;
                return Ok(Track {
                    timescale: timescale.context("no timescale")?,
                    width,
                    height,
                    nal_length,
                    parameters,
                    fragments_at: at,
                });
            }
            _ => {}
        }
        at = end;
    }
    bail!("the init segment ended before the first fragment")
}

/// One segment the file's index names: when its first picture is shown and the byte its `moof`
/// starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub start: Duration,
    pub offset: usize,
}

/// Reads the segment index at `at`, where [`Track::fragments_at`] points. `None` while the
/// index has not fully arrived. A file with no index there answers with no segments, and is
/// then read from its start.
pub fn read_index(data: &[u8], at: usize) -> Result<Option<Vec<Segment>>> {
    let Some(header) = data.get(at..at + 8) else {
        return Ok(None);
    };
    if &header[4..8] != b"sidx" {
        return Ok(Some(Vec::new()));
    }
    let Some((_, body, end)) = next_box(data, at) else {
        return Ok(None);
    };
    let sidx = &data[body];
    let timescale = read_u32(sidx, 8)?.max(1) as f64;
    let (mut ticks, first, mut cursor) = match sidx.first() {
        Some(0) => (read_u32(sidx, 12)? as u64, read_u32(sidx, 16)? as u64, 20),
        _ => (read_u64(sidx, 12)?, read_u64(sidx, 20)?, 28),
    };
    let count = u16::from_be_bytes(
        sidx.get(cursor + 2..cursor + 4)
            .context("the index is cut short")?
            .try_into()?,
    );
    cursor += 4;
    let mut offset = end + usize::try_from(first).context("the index offset does not fit")?;
    let mut segments = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let size = read_u32(sidx, cursor)? & 0x7fff_ffff;
        let duration = read_u32(sidx, cursor + 4)?;
        segments.push(Segment {
            start: Duration::from_secs_f64(ticks as f64 / timescale),
            offset,
        });
        offset += size as usize;
        ticks += duration as u64;
        cursor += 12;
    }
    Ok(Some(segments))
}

/// Reads the fragment whose `moof` starts at `at`. `None` while its `moof` or `mdat` has not
/// fully arrived. Boxes between fragments, such as a `sidx`, are stepped over.
pub fn read_fragment(data: &[u8], mut at: usize) -> Result<Option<Fragment>> {
    let (moof, end) = loop {
        let Some((kind, body, end)) = next_box(data, at) else {
            return Ok(None);
        };
        if &kind == b"moof" {
            break (body, end);
        }
        at = end;
    };
    let moof_start = at;
    let Some((kind, _, next)) = next_box(data, end) else {
        return Ok(None);
    };
    if &kind != b"mdat" {
        bail!(
            "a fragment's moof is followed by {}",
            String::from_utf8_lossy(&kind)
        );
    }
    let traf = path(&data[moof.clone()], &[b"traf"]).context("the fragment has no traf")?;
    let traf = &data[moof.start + traf.start..moof.start + traf.end];
    let samples = traf_samples(traf, moof_start)?;
    if samples.iter().any(|sample| sample.bytes.end > next) {
        bail!("a sample runs past its mdat");
    }
    Ok(Some(Fragment { samples, next }))
}

/// A sample's NAL units with start codes in front, the way the decoder takes them, with the
/// parameter sets ahead of a key frame.
pub fn annex_b(track: &Track, sample: &Sample, data: &[u8]) -> Result<Vec<u8>> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::with_capacity(sample.bytes.len() + 64);
    if sample.key {
        for set in &track.parameters {
            out.extend_from_slice(&START);
            out.extend_from_slice(set);
        }
    }
    let mut rest = &data[sample.bytes.clone()];
    while !rest.is_empty() {
        if rest.len() < track.nal_length {
            bail!("a NAL length is cut short");
        }
        let length = rest[..track.nal_length]
            .iter()
            .fold(0usize, |length, byte| length << 8 | *byte as usize);
        rest = &rest[track.nal_length..];
        if length > rest.len() {
            bail!("a NAL unit runs past its sample");
        }
        out.extend_from_slice(&START);
        out.extend_from_slice(&rest[..length]);
        rest = &rest[length..];
    }
    Ok(out)
}

/// The type, body and end of the box at `at`, `None` when it has not fully arrived.
fn next_box(data: &[u8], at: usize) -> Option<([u8; 4], Range<usize>, usize)> {
    let header = data.get(at..at + 8)?;
    let size = u32::from_be_bytes(header[..4].try_into().ok()?) as u64;
    let kind: [u8; 4] = header[4..8].try_into().ok()?;
    let (size, body) = match size {
        1 => (
            u64::from_be_bytes(data.get(at + 8..at + 16)?.try_into().ok()?),
            at + 16,
        ),
        0 => ((data.len() - at) as u64, at + 8),
        size => (size, at + 8),
    };
    let end = at.checked_add(usize::try_from(size).ok()?)?;
    (end <= data.len() && size >= 8).then_some((kind, body..end, end))
}

/// The body of the box found by walking `kinds` from the boxes in `data`, relative to it.
fn path(data: &[u8], kinds: &[&[u8; 4]]) -> Option<Range<usize>> {
    let mut range = 0..data.len();
    for kind in kinds {
        let mut at = range.start;
        range = loop {
            let (found, body, end) = next_box(&data[..range.end], at)?;
            if &found == *kind {
                break body;
            }
            at = end;
        };
    }
    Some(range)
}

fn mdhd_timescale(mdia: &[u8]) -> Result<u32> {
    let mdhd = path(mdia, &[b"mdhd"]).context("the track has no mdhd")?;
    let mdhd = &mdia[mdhd];
    let at = match mdhd.first() {
        Some(1) => 20,
        _ => 12,
    };
    read_u32(mdhd, at)
}

/// The picture size, NAL length size and parameter sets of an `avc1` sample entry.
fn avc1(stsd: &[u8]) -> Result<(u32, u32, usize, Vec<Vec<u8>>)> {
    // The full box header and the entry count come before the first entry.
    let (kind, body, _) = next_box(stsd, 8).context("the description has no entry")?;
    if &kind != b"avc1" && &kind != b"avc3" {
        bail!("the video is {}, not h.264", String::from_utf8_lossy(&kind));
    }
    let entry = &stsd[body];
    let width = u16::from_be_bytes(entry.get(24..26).context("short avc1")?.try_into()?) as u32;
    let height = u16::from_be_bytes(entry.get(26..28).context("short avc1")?.try_into()?) as u32;
    // The visual sample entry is 78 bytes before its child boxes.
    let avcc = path(&entry[78..], &[b"avcC"]).context("the entry has no avcC")?;
    let avcc = &entry[78 + avcc.start..78 + avcc.end];
    let nal_length = (*avcc.get(4).context("short avcC")? & 3) as usize + 1;
    let mut parameters = Vec::new();
    let mut at = 5;
    for mask in [0x1f, 0xff] {
        let count = *avcc.get(at).context("short avcC")? & mask;
        at += 1;
        for _ in 0..count {
            let length =
                u16::from_be_bytes(avcc.get(at..at + 2).context("short avcC")?.try_into()?);
            at += 2;
            let set = avcc
                .get(at..at + length as usize)
                .context("a parameter set runs past avcC")?;
            parameters.push(set.to_vec());
            at += length as usize;
        }
    }
    Ok((width, height, nal_length, parameters))
}

/// The samples a `traf` describes, placed in the file by the `moof` starting at `moof_start`.
fn traf_samples(traf: &[u8], moof_start: usize) -> Result<Vec<Sample>> {
    let tfhd = path(traf, &[b"tfhd"]).context("the fragment has no tfhd")?;
    let tfhd = &traf[tfhd];
    let flags = read_u32(tfhd, 0)? & 0xff_ffff;
    let mut at = 8;
    let mut base = moof_start as u64;
    if flags & 0x1 != 0 {
        base = read_u64(tfhd, at)?;
        at += 8;
    }
    if flags & 0x2 != 0 {
        at += 4;
    }
    let mut default_duration = 0;
    let mut default_size = 0;
    let mut default_flags = 0;
    if flags & 0x8 != 0 {
        default_duration = read_u32(tfhd, at)?;
        at += 4;
    }
    if flags & 0x10 != 0 {
        default_size = read_u32(tfhd, at)?;
        at += 4;
    }
    if flags & 0x20 != 0 {
        default_flags = read_u32(tfhd, at)?;
    }
    let mut decode = match path(traf, &[b"tfdt"]) {
        Some(tfdt) => {
            let tfdt = &traf[tfdt];
            match tfdt.first() {
                Some(1) => read_u64(tfdt, 4)?,
                _ => read_u32(tfdt, 4)? as u64,
            }
        }
        None => 0,
    };

    let trun = path(traf, &[b"trun"]).context("the fragment has no trun")?;
    let trun = &traf[trun];
    let version = trun[0];
    let flags = read_u32(trun, 0)? & 0xff_ffff;
    let count = read_u32(trun, 4)?;
    let mut at = 8;
    let mut offset = base;
    if flags & 0x1 != 0 {
        offset = (base as i64 + read_u32(trun, at)? as i32 as i64) as u64;
        at += 4;
    }
    let mut first_flags = None;
    if flags & 0x4 != 0 {
        first_flags = Some(read_u32(trun, at)?);
        at += 4;
    }
    let mut samples = Vec::with_capacity(count as usize);
    for index in 0..count {
        let mut field = |present: u32| -> Result<Option<u32>> {
            if flags & present == 0 {
                return Ok(None);
            }
            let value = read_u32(trun, at)?;
            at += 4;
            Ok(Some(value))
        };
        let duration = field(0x100)?.unwrap_or(default_duration);
        let size = field(0x200)?.unwrap_or(default_size);
        let sample_flags = field(0x400)?
            .or(first_flags.filter(|_| index == 0))
            .unwrap_or(default_flags);
        let shift = match (field(0x800)?, version) {
            (Some(shift), 0) => shift as i64,
            (Some(shift), _) => shift as i32 as i64,
            (None, _) => 0,
        };
        let start = usize::try_from(offset).context("a sample offset does not fit")?;
        samples.push(Sample {
            bytes: start..start + size as usize,
            decode,
            show: (decode as i64 + shift).max(0) as u64,
            duration,
            // The sample_is_non_sync_sample bit.
            key: sample_flags & 0x1_0000 == 0,
        });
        offset += size as u64;
        decode += duration as u64;
    }
    Ok(samples)
}

fn read_u32(data: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        data.get(at..at + 4)
            .context("a box is cut short")?
            .try_into()?,
    ))
}

fn read_u64(data: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(
        data.get(at..at + 8)
            .context("a box is cut short")?
            .try_into()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn a_fragment_places_its_samples_after_the_moof() {
        // tfhd with a default duration of 1000 and default-base-is-moof.
        let mut tfhd = vec![0, 0x02, 0, 0x08, 0, 0, 0, 1];
        tfhd.extend_from_slice(&1000u32.to_be_bytes());
        let tfdt = [vec![0, 0, 0, 0], 5000u32.to_be_bytes().to_vec()].concat();
        // trun with a data offset, per-sample sizes and flags, two samples.
        let mut trun = vec![0, 0, 0x06, 0x01];
        trun.extend_from_slice(&2u32.to_be_bytes());
        let traf_len = 8 + (8 + tfhd.len()) + (8 + tfdt.len()) + (8 + 12 + 16);
        let moof_len = 8 + traf_len;
        trun.extend_from_slice(&((moof_len + 8) as u32).to_be_bytes());
        for (size, flags) in [(6u32, 0u32), (5, 0x1_0000)] {
            trun.extend_from_slice(&size.to_be_bytes());
            trun.extend_from_slice(&flags.to_be_bytes());
        }
        let traf = boxed(
            b"traf",
            &[
                boxed(b"tfhd", &tfhd),
                boxed(b"tfdt", &tfdt),
                boxed(b"trun", &trun),
            ]
            .concat(),
        );
        let moof = boxed(b"moof", &traf);
        assert_eq!(moof.len(), moof_len);
        let mdat = boxed(b"mdat", &[0, 0, 0, 2, 9, 9, 0, 0, 0, 1, 7]);
        let file = [moof, mdat].concat();

        let fragment = read_fragment(&file, 0).unwrap().unwrap();
        assert_eq!(fragment.next, file.len());
        let first = &fragment.samples[0];
        assert_eq!(first.bytes, moof_len + 8..moof_len + 14);
        assert_eq!((first.decode, first.key), (5000, true));
        assert_eq!(
            (fragment.samples[1].decode, fragment.samples[1].key),
            (6000, false)
        );

        let track = Track {
            timescale: 1000,
            width: 0,
            height: 0,
            nal_length: 4,
            parameters: vec![vec![0x67], vec![0x68]],
            fragments_at: 0,
        };
        assert_eq!(
            annex_b(&track, first, &file).unwrap(),
            [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 9, 9]
        );
    }

    #[test]
    fn a_fragment_still_arriving_reads_as_none() {
        let moof = boxed(b"moof", &[0; 16]);
        assert!(read_fragment(&moof[..10], 0).unwrap().is_none());
    }
}
