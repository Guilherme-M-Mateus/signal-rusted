//! Voice notes: MP3 encoding, the playback clock, and the device thread (rodio/cpal).
//!
//! The UI talks to the device thread only through [`Cmd`], so app logic is testable with a
//! captured channel and no audio hardware.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rodio::Source;
use tokio::sync::oneshot;

pub type Reply<T> = oneshot::Sender<Result<T, String>>;

#[derive(Debug)]
pub enum Cmd {
    Play(PathBuf),
    Pause,
    Resume,
    Record(Reply<()>),
    /// Ends the recording: `Some` encodes it to that path, `None` discards it.
    Finish(Option<(PathBuf, Reply<PathBuf>)>),
}

/// Where a playing (or paused) attachment is, by wall clock: the device thread reports nothing back.
#[derive(Debug)]
pub struct Playback {
    pub key: String,
    played: Duration,
    since: Option<Instant>,
}

impl Playback {
    pub fn new(key: String, now: Instant) -> Self {
        Playback { key, played: Duration::ZERO, since: Some(now) }
    }

    pub fn position(&self, now: Instant) -> Duration {
        self.played + self.since.map_or(Duration::ZERO, |s| now.duration_since(s))
    }

    pub fn is_paused(&self) -> bool {
        self.since.is_none()
    }

    pub fn pause(&mut self, now: Instant) {
        self.played = self.position(now);
        self.since = None;
    }

    pub fn resume(&mut self, now: Instant) {
        self.since.get_or_insert(now);
    }

    pub fn finished(&self, now: Instant, total: Option<Duration>) -> bool {
        total.is_some_and(|t| self.position(now) >= t)
    }
}

pub fn mmss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// Mono MP3, what Signal Desktop sends for voice notes and phones play natively.
pub fn encode_mp3(samples: &[f32], rate: u32) -> anyhow::Result<Vec<u8>> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, Mode, MonoPcm, Quality};
    let err = |e: &dyn std::fmt::Display| anyhow::anyhow!("mp3 encoder: {e}");
    let mut b = Builder::new().ok_or_else(|| anyhow::anyhow!("mp3 encoder unavailable"))?;
    b.set_num_channels(1).map_err(|e| err(&e))?;
    b.set_sample_rate(rate).map_err(|e| err(&e))?;
    b.set_mode(Mode::Mono).map_err(|e| err(&e))?;
    b.set_brate(Bitrate::Kbps64).map_err(|e| err(&e))?;
    b.set_quality(Quality::Good).map_err(|e| err(&e))?;
    let mut enc = b.build().map_err(|e| err(&e))?;
    let mut out = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(samples.len()) + 7200);
    enc.encode_to_vec(MonoPcm(samples), &mut out).map_err(|e| err(&e))?;
    enc.flush_to_vec::<FlushNoGap>(&mut out).map_err(|e| err(&e))?;
    Ok(out)
}

/// Length of an audio file, decoding it when the container doesn't say (e.g. MP3).
pub fn duration(path: &Path) -> Option<Duration> {
    let dec = rodio::Decoder::try_from(std::fs::File::open(path).ok()?).ok()?;
    if let Some(d) = dec.total_duration() {
        return Some(d);
    }
    let (ch, rate) = (dec.channels().get() as f64, dec.sample_rate().get() as f64);
    Some(Duration::from_secs_f64(dec.count() as f64 / ch / rate))
}

/// Starts the device thread. Device-dependent: unverified in tests (no audio hardware there).
pub fn spawn() -> mpsc::Sender<Cmd> {
    debug_assert!(!cfg!(test), "tests must inject a fake audio channel");
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("audio".into()).spawn(move || device(rx)).expect("spawn audio thread");
    tx
}

/// What the capture thread has so far: finished streams, plus the current one in progress.
#[derive(Default)]
struct Captured {
    rec: (Vec<f32>, u32),
    current: Vec<f32>,
    rate: u32,
}

type Capture = (Arc<AtomicBool>, Arc<std::sync::Mutex<Captured>>, std::thread::JoinHandle<()>);

fn lock(m: &std::sync::Mutex<Captured>) -> std::sync::MutexGuard<'_, Captured> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stops a capture and returns its samples. A stalled stream (rodio blocks until the next sample)
/// must never hang this thread, so it waits briefly and then takes what was captured so far.
fn stop_capture((stop, shared, h): Capture) -> (Vec<f32>, u32) {
    stop.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !h.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if !h.is_finished() {
        tracing::warn!("microphone stalled; using what was recorded");
    }
    let mut c = lock(&shared);
    let (current, rate) = (std::mem::take(&mut c.current), c.rate);
    let mut rec = std::mem::take(&mut c.rec);
    merge_stream(&mut rec, current, rate, false);
    rec
}

fn device(rx: mpsc::Receiver<Cmd>) {
    let mut sink: Option<rodio::MixerDeviceSink> = None;
    let mut player: Option<rodio::Player> = None;
    let mut capture: Option<Capture> = None;
    for cmd in rx {
        match cmd {
            Cmd::Play(path) => {
                player = None;
                if sink.is_none() {
                    sink = rodio::DeviceSinkBuilder::open_default_sink()
                        .inspect_err(|e| tracing::warn!(%e, "no audio output"))
                        .ok()
                        .map(|mut s| {
                            s.log_on_drop(false);
                            s
                        });
                }
                let Some(s) = &sink else { continue };
                match std::fs::File::open(&path).map_err(|e| e.to_string()).and_then(|f| rodio::Decoder::try_from(f).map_err(|e| e.to_string())) {
                    Ok(dec) => {
                        let p = rodio::Player::connect_new(s.mixer());
                        p.append(dec);
                        player = Some(p);
                    }
                    Err(e) => tracing::warn!(%e, ?path, "can't play audio"),
                }
            }
            Cmd::Pause => player.iter().for_each(|p| p.pause()),
            Cmd::Resume => player.iter().for_each(|p| p.play()),
            Cmd::Record(reply) => {
                if let Some(c) = capture.take() {
                    stop_capture(c);
                }
                capture = Some(record(reply));
            }
            Cmd::Finish(out) => {
                let Some(c) = capture.take() else { continue };
                let (samples, rate) = stop_capture(c);
                if let Some((path, reply)) = out {
                    let r = check_recording(&samples, rate).map_err(anyhow::Error::msg).and_then(|()| {
                        std::fs::write(&path, encode_mp3(&samples, rate)?)?;
                        Ok(path)
                    });
                    let _ = reply.send(r.map_err(|e| format!("{e:#}")));
                }
            }
        }
    }
}

/// Refuses what isn't a voice note: too short, or silence (a mic that delivered only zeros).
pub fn check_recording(samples: &[f32], rate: u32) -> Result<(), String> {
    if rate == 0 || samples.len() < rate as usize / 2 {
        return Err("Recording too short (hold the mic a little longer)".into());
    }
    if samples.iter().all(|s| s.abs() < 1e-4) {
        return Err("No sound was recorded. Check your microphone; Bluetooth headsets must switch to headset mode first".into());
    }
    Ok(())
}

/// Adds one stream's samples to the recording. A stream that died while still silent (e.g. a
/// Bluetooth headset switching to headset mode) contributes nothing; a rate change starts over.
pub fn merge_stream(acc: &mut (Vec<f32>, u32), samples: Vec<f32>, rate: u32, died: bool) {
    if died && samples.iter().all(|s| s.abs() < 1e-4) {
        return;
    }
    if acc.1 != rate {
        *acc = (Vec::new(), rate);
    }
    acc.0.extend(samples);
}

fn open_mic() -> Result<rodio::microphone::Microphone, String> {
    rodio::microphone::MicrophoneBuilder::new()
        .default_device()
        .and_then(|b| b.default_config())
        .map_err(|e| e.to_string())
        .and_then(|b| b.prefer_sample_rates([48_000, 44_100].map(|r| r.try_into().expect("nonzero"))).open_stream().map_err(|e| e.to_string()))
}

/// Captures the default mic as mono on its own thread (cpal streams aren't `Send`). The stream is
/// reopened if it dies mid-recording, as it does while a Bluetooth headset changes profile.
fn record(reply: Reply<()>) -> Capture {
    const REOPENS: usize = 5;
    /// Samples between publishing progress, so a stall can't lose more than ~0.1 s.
    const CHUNK: usize = 4_800;
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(std::sync::Mutex::new(Captured::default()));
    let (flag, out) = (stop.clone(), shared.clone());
    let h = std::thread::spawn(move || {
        let mut reply = Some(reply);
        for attempt in 0..=REOPENS {
            let mic = match open_mic() {
                Ok(m) => m,
                Err(e) if attempt == 0 => {
                    let _ = reply.take().map(|r| r.send(Err(format!("Microphone unavailable: {e}"))));
                    return;
                }
                Err(e) => {
                    tracing::warn!("couldn't reopen the microphone: {e}");
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
            };
            let _ = reply.take().map(|r| r.send(Ok(())));
            let (ch, rate) = (mic.channels().get() as usize, mic.sample_rate().get());
            lock(&out).rate = rate;
            let mut chunk = Vec::with_capacity(CHUNK);
            let mut frame = Vec::with_capacity(ch);
            for s in mic {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                frame.push(s);
                if frame.len() == ch {
                    chunk.push(frame.drain(..).sum::<f32>() / ch as f32);
                    if chunk.len() == CHUNK {
                        lock(&out).current.append(&mut chunk);
                    }
                }
            }
            let died = !flag.load(Ordering::Relaxed);
            let mut c = lock(&out);
            c.current.append(&mut chunk);
            if !died {
                return; // stop_capture merges the last stream
            }
            let current = std::mem::take(&mut c.current);
            merge_stream(&mut c.rec, current, rate, true);
            drop(c);
            tracing::warn!(attempt, "microphone stream stopped; reopening");
        }
    });
    (stop, shared, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp3_round_trip_keeps_length() {
        let rate = 48_000;
        let sine: Vec<f32> = (0..rate * 2).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5).collect();
        let mp3 = encode_mp3(&sine, rate).unwrap();
        assert!(mp3.len() > 10_000 && mp3.len() < 30_000, "~64 kbps for 2 s, got {}", mp3.len());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.mp3");
        std::fs::write(&path, &mp3).unwrap();
        let d = duration(&path).unwrap();
        assert!((d.as_secs_f64() - 2.0).abs() < 0.1, "decoded {d:?}");

        let dec = rodio::Decoder::try_from(std::io::Cursor::new(mp3)).unwrap();
        assert_eq!((dec.channels().get(), dec.sample_rate().get()), (1, rate));
        let decoded: Vec<f32> = dec.collect();
        assert!((decoded.len() as i64 - sine.len() as i64).abs() < 4_000, "{} vs {}", decoded.len(), sine.len());
        let rms = (decoded.iter().map(|s| s * s).sum::<f32>() / decoded.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.05, "signal survives, rms {rms}");
    }

    #[test]
    fn silent_or_short_recordings_are_refused() {
        let rate = 48_000;
        assert!(check_recording(&vec![0.0; rate as usize], rate).unwrap_err().contains("No sound"));
        assert!(check_recording(&vec![0.2; 1_000], rate).unwrap_err().contains("too short"));
        assert!(check_recording(&[], 0).is_err());
        assert!(check_recording(&vec![0.2; rate as usize], rate).is_ok());
    }

    #[test]
    fn dead_silent_streams_are_dropped_and_rate_changes_restart() {
        let mut rec = (Vec::new(), 0);
        merge_stream(&mut rec, vec![0.0; 26_000], 48_000, true); // Bluetooth switching: silence, then died
        assert!(rec.0.is_empty());
        merge_stream(&mut rec, vec![0.3; 100], 48_000, true); // died with real audio: kept
        merge_stream(&mut rec, vec![0.1; 50], 48_000, false);
        assert_eq!((rec.0.len(), rec.1), (150, 48_000));
        merge_stream(&mut rec, vec![0.1; 10], 16_000, false); // headset mode at another rate
        assert_eq!((rec.0.len(), rec.1), (10, 16_000), "can't mix rates");
    }

    #[test]
    fn playback_clock_plays_pauses_and_finishes() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        let total = Some(Duration::from_secs(10));
        let mut p = Playback::new("k".into(), t0);
        assert_eq!(p.position(s(3)), Duration::from_secs(3));
        p.pause(s(4));
        assert!(p.is_paused());
        assert_eq!(p.position(s(100)), Duration::from_secs(4), "paused clock stands still");
        p.resume(s(100));
        p.resume(s(101)); // idempotent
        assert!(!p.finished(s(105), total));
        assert!(p.finished(s(106), total));
        assert!(!p.finished(s(1000), None), "unknown length never auto-finishes");
        assert_eq!(mmss(Duration::from_secs(75)), "1:15");
    }
}
