//! Reads the boxes of a DASH video file and prints what the demuxer found:
//! `cargo run -p video --example inspect -- video.mp4`.

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "video.mp4".into());
    let data = std::fs::read(path)?;
    let track = video::fmp4::read_init(&data)?;
    println!(
        "{}x{} timescale {} nal length {} parameter sets {:?}",
        track.width,
        track.height,
        track.timescale,
        track.nal_length,
        track.parameters.iter().map(Vec::len).collect::<Vec<_>>()
    );
    let mut at = track.fragments_at;
    let (mut fragments, mut samples, mut keys) = (0, 0, 0);
    let mut last = 0;
    while let Some(fragment) = video::fmp4::read_fragment(&data, at)? {
        if fragments < 2 {
            let first = &fragment.samples[0];
            println!(
                "fragment {fragments}: {} samples, first at {} key {} {} bytes",
                fragment.samples.len(),
                first.show,
                first.key,
                first.bytes.len()
            );
            let nal = video::fmp4::annex_b(&track, first, &data)?;
            println!("  annex b {} bytes, starts {:02x?}", nal.len(), &nal[..8]);
        }
        fragments += 1;
        samples += fragment.samples.len();
        keys += fragment.samples.iter().filter(|sample| sample.key).count();
        last = fragment.samples.last().map_or(last, |sample| sample.show);
        at = fragment.next;
    }
    println!(
        "{fragments} fragments, {samples} samples, {keys} key frames, ends at {:.2}s, read to byte {at} of {}",
        last as f64 / track.timescale as f64,
        data.len()
    );
    Ok(())
}
