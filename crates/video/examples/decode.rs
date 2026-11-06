//! Decodes a YouTube DASH video file from a few places and saves a frame from each as a PNG,
//! timing the decoder: `cargo run -p video --example decode -- video.mp4 out-dir`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use video::fmp4;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let mut args = std::env::args().skip(1);
    let input = PathBuf::from(args.next().unwrap_or_else(|| "video.mp4".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;

    let data = std::fs::read(&input)?;
    let track = fmp4::read_init(&data)?;
    let mut fragments = Vec::new();
    let mut at = track.fragments_at;
    while let Some(fragment) = fmp4::read_fragment(&data, at)? {
        at = fragment.next;
        fragments.push(fragment);
    }
    let seconds = |ticks: u64| Duration::from_secs_f64(ticks as f64 / track.timescale as f64);

    let started = Instant::now();
    let mut decoder = video::Decoder::new(&track)?;
    println!("decoder ready in {:?}", started.elapsed());

    for target in [
        Duration::ZERO,
        Duration::from_secs(60),
        Duration::from_secs(150),
    ] {
        // The fragment that covers the target, which starts on a key frame.
        let fragment = fragments
            .iter()
            .rposition(|fragment| seconds(fragment.samples[0].show) <= target)
            .unwrap_or(0);
        decoder.flush()?;
        let started = Instant::now();
        let mut decoded = 0;
        let mut shown = None;
        'feed: for fragment in &fragments[fragment..] {
            for sample in &fragment.samples {
                let nal = fmp4::annex_b(&track, sample, &data)?;
                let frames = decoder.push(
                    &nal,
                    seconds(sample.show),
                    seconds(sample.duration as u64),
                    sample.key,
                )?;
                decoded += frames.len();
                if let Some(frame) = frames.into_iter().find(|frame| frame.at >= target) {
                    shown = Some(frame);
                    break 'feed;
                }
            }
        }
        let elapsed = started.elapsed();
        let Some(frame) = shown else {
            println!("no frame reached {target:?}");
            continue;
        };
        println!(
            "{target:?}: frame at {:?} after decoding {decoded} frames in {elapsed:?} ({:.1} ms each)",
            frame.at,
            elapsed.as_secs_f64() * 1000. / decoded.max(1) as f64
        );
        let mut rgba = frame.bgra;
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let image = image::RgbaImage::from_raw(frame.width, frame.height, rgba)
            .ok_or_else(|| anyhow::anyhow!("the frame does not fit its size"))?;
        let path = out.join(format!("frame-{}s.png", target.as_secs()));
        image.save(&path)?;
        println!("saved {}", path.display());
    }
    Ok(())
}
