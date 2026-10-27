//! Diagnostic: captures 2 s from the default mic the same way voice notes do and prints only
//! format, sample count and level (nothing is stored or sent). `cargo run --example micprobe`
use std::time::{Duration, Instant};

fn main() {
    let b = rodio::microphone::MicrophoneBuilder::new().default_device().and_then(|b| b.default_config());
    let b = match b { Ok(b) => b, Err(e) => return println!("no default mic/config: {e}") };
    let mic = b
        .prefer_sample_rates([48_000, 44_100].map(|r| r.try_into().unwrap()))
        .open_stream();
    let mic = match mic { Ok(m) => m, Err(e) => return println!("open_stream failed: {e}") };
    use rodio::Source;
    let (ch, rate) = (mic.channels().get(), mic.sample_rate().get());
    println!("channels={ch} rate={rate}");
    let start = Instant::now();
    let (mut n, mut sq, mut peak) = (0usize, 0f64, 0f32);
    for s in mic {
        n += 1;
        sq += (s as f64) * (s as f64);
        peak = peak.max(s.abs());
        if start.elapsed() > Duration::from_secs(2) { break; }
    }
    println!("ended after {:?}: samples={n} (expected ~{}) rms={:.5} peak={:.4}",
        start.elapsed(), 2 * rate as usize * ch as usize, (sq / n.max(1) as f64).sqrt(), peak);
}
