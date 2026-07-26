//! Native audio engine: Symphonia decoding → optional resampling → EQ → cpal.
//!
//! Threading model, chosen so the output callback never blocks or allocates:
//!
//! * **decoder thread** – owns the format reader and decoder, converts packets
//!   to f32, resamples when the device rate differs, and pushes interleaved
//!   frames into a lock-free ring buffer.
//! * **output thread** – owns the cpal stream (not `Send` on Windows, so it
//!   must live on one thread). Its callback pops frames, applies the EQ and
//!   volume, and forwards a mono mix to the analyser ring.
//! * **monitor thread** – computes the spectrum with an FFT and emits progress
//!   events to the UI ~30×/s.
//!
//! Playback position is derived from a marker queue rather than a single
//! counter, so it stays correct across gapless track changes: the decoder
//! records where in the global frame stream each track begins, and the callback
//! resolves the current position from whichever marker it has passed.

pub mod eq;
pub mod http_source;
pub mod probe;

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use realfft::RealFftPlanner;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use serde::Serialize;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use tauri::{AppHandle, Emitter};

use eq::{Equalizer, BANDS};
use http_source::HttpSource;

/// Ring capacity in frames (~1.5 s at 48 kHz) — enough to ride out scheduling
/// hiccups without adding noticeable latency to transport controls.
const RING_FRAMES: usize = 72_000;
/// Frames handed to the resampler at a time.
const RESAMPLE_CHUNK: usize = 1024;
const FFT_SIZE: usize = 1024;
const SPECTRUM_BARS: usize = 64;

#[derive(Clone, Debug)]
pub enum Source {
    File(PathBuf),
    Url(String),
}

impl Source {
    pub fn parse(s: &str) -> Self {
        if s.starts_with("http://") || s.starts_with("https://") {
            Source::Url(s.to_string())
        } else {
            Source::File(PathBuf::from(s))
        }
    }

    fn hint(&self) -> Hint {
        let mut hint = Hint::new();
        let ext = match self {
            Source::File(p) => p
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase()),
            Source::Url(u) => u
                .split('?')
                .next()
                .and_then(|base| base.rsplit('.').next())
                .filter(|e| e.len() <= 5)
                .map(|s| s.to_ascii_lowercase()),
        };
        if let Some(e) = ext {
            hint.with_extension(&e);
        }
        hint
    }
}

enum Cmd {
    Load {
        source: Source,
        track_id: u64,
        autoplay: bool,
    },
    SetNext(Option<(Source, u64)>),
    Play,
    Pause,
    Seek(f64),
    Stop,
}

/// Marker telling the output callback that a new track starts at a given point
/// in the global frame stream.
#[derive(Clone, Copy)]
struct TrackMarker {
    at_global_frame: u64,
    track_id: u64,
    /// Position inside the track at that point — non-zero after a seek.
    track_start_frame: u64,
}

pub struct Shared {
    playing: AtomicBool,
    /// Frames the callback has delivered to the device since start-up.
    played_frames: AtomicU64,
    /// Position within the current track, in output frames.
    pos_frames: AtomicU64,
    duration_ms: AtomicU64,
    current_track: AtomicU64,
    ended: AtomicBool,
    volume_bits: AtomicU32,
    eq_enabled: AtomicBool,
    preamp_bits: AtomicU32,
    eq_gain_bits: [AtomicU32; BANDS],
    eq_version: AtomicU64,
    /// Bumped by the decoder to tell the callback to throw away buffered audio
    /// (after a seek or a new track) — only the consumer side can empty a ring.
    flush_version: AtomicU64,
    out_rate: AtomicU32,
    out_channels: AtomicU32,
    /// Source sample rate / bit depth of what is playing, for the UI.
    src_rate: AtomicU32,
    src_bits: AtomicU32,
    spectrum: Mutex<[f32; SPECTRUM_BARS]>,
    last_error: Mutex<Option<String>>,
    codec: Mutex<String>,
}

impl Shared {
    fn new() -> Self {
        Self {
            playing: AtomicBool::new(false),
            played_frames: AtomicU64::new(0),
            pos_frames: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            current_track: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
            eq_enabled: AtomicBool::new(true),
            preamp_bits: AtomicU32::new(0.0f32.to_bits()),
            eq_gain_bits: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            eq_version: AtomicU64::new(0),
            flush_version: AtomicU64::new(0),
            out_rate: AtomicU32::new(48_000),
            out_channels: AtomicU32::new(2),
            src_rate: AtomicU32::new(0),
            src_bits: AtomicU32::new(0),
            spectrum: Mutex::new([0.0; SPECTRUM_BARS]),
            last_error: Mutex::new(None),
            codec: Mutex::new(String::new()),
        }
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    fn eq_params(&self) -> ([f32; BANDS], f32, bool) {
        let mut gains = [0.0f32; BANDS];
        for (i, slot) in self.eq_gain_bits.iter().enumerate() {
            gains[i] = f32::from_bits(slot.load(Ordering::Relaxed));
        }
        (
            gains,
            f32::from_bits(self.preamp_bits.load(Ordering::Relaxed)),
            self.eq_enabled.load(Ordering::Relaxed),
        )
    }
}

#[derive(Serialize, Clone)]
pub struct EngineState {
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub track_id: u64,
    pub ended: bool,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub codec: String,
    pub error: Option<String>,
}

#[derive(Serialize, Clone)]
struct Tick {
    playing: bool,
    position: f64,
    duration: f64,
    track_id: u64,
    ended: bool,
    bars: Vec<u8>,
    sample_rate: u32,
    bit_depth: u32,
    codec: String,
}

pub struct AudioEngine {
    cmd_tx: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    next_track_id: AtomicU64,
}

impl AudioEngine {
    pub fn new(app: AppHandle) -> Result<Self, String> {
        let shared = Arc::new(Shared::new());

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no audio output device found".to_string())?;
        let config = device
            .default_output_config()
            .map_err(|e| format!("output config unavailable: {e}"))?;
        let out_rate = config.sample_rate();
        let out_channels = config.channels() as usize;
        shared.out_rate.store(out_rate, Ordering::Relaxed);
        shared
            .out_channels
            .store(out_channels as u32, Ordering::Relaxed);

        let (audio_tx, audio_rx) = rtrb::RingBuffer::<f32>::new(RING_FRAMES * out_channels);
        let (marker_tx, marker_rx) = rtrb::RingBuffer::<TrackMarker>::new(64);
        let (fft_tx, fft_rx) = rtrb::RingBuffer::<f32>::new(FFT_SIZE * 8);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        spawn_output_thread(
            device,
            config,
            audio_rx,
            marker_rx,
            fft_tx,
            Arc::clone(&shared),
        );
        spawn_decoder_thread(
            cmd_rx,
            audio_tx,
            marker_tx,
            Arc::clone(&shared),
            out_rate,
            out_channels,
        );
        spawn_monitor_thread(app, fft_rx, Arc::clone(&shared));

        Ok(Self {
            cmd_tx,
            shared,
            next_track_id: AtomicU64::new(1),
        })
    }

    fn send(&self, cmd: Cmd) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| "audio engine is not running".to_string())
    }

    pub fn load(&self, path: &str, autoplay: bool) -> Result<u64, String> {
        let track_id = self.next_track_id.fetch_add(1, Ordering::SeqCst);
        self.shared.ended.store(false, Ordering::Relaxed);
        self.send(Cmd::Load {
            source: Source::parse(path),
            track_id,
            autoplay,
        })?;
        Ok(track_id)
    }

    pub fn set_next(&self, path: Option<&str>) -> Result<u64, String> {
        match path {
            Some(p) => {
                let track_id = self.next_track_id.fetch_add(1, Ordering::SeqCst);
                self.send(Cmd::SetNext(Some((Source::parse(p), track_id))))?;
                Ok(track_id)
            }
            None => {
                self.send(Cmd::SetNext(None))?;
                Ok(0)
            }
        }
    }

    pub fn play(&self) -> Result<(), String> {
        self.send(Cmd::Play)
    }

    pub fn pause(&self) -> Result<(), String> {
        self.send(Cmd::Pause)
    }

    pub fn stop(&self) -> Result<(), String> {
        self.send(Cmd::Stop)
    }

    pub fn seek(&self, secs: f64) -> Result<(), String> {
        self.send(Cmd::Seek(secs.max(0.0)))
    }

    pub fn set_volume(&self, v: f32) {
        self.shared
            .volume_bits
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_eq(&self, gains: &[f32], preamp: f32, enabled: bool) {
        for (i, slot) in self.shared.eq_gain_bits.iter().enumerate() {
            let g = gains.get(i).copied().unwrap_or(0.0).clamp(-24.0, 24.0);
            slot.store(g.to_bits(), Ordering::Relaxed);
        }
        self.shared
            .preamp_bits
            .store(preamp.clamp(-24.0, 24.0).to_bits(), Ordering::Relaxed);
        self.shared.eq_enabled.store(enabled, Ordering::Relaxed);
        self.shared.eq_version.fetch_add(1, Ordering::Release);
    }

    pub fn state(&self) -> EngineState {
        let rate = self.shared.out_rate.load(Ordering::Relaxed).max(1) as f64;
        EngineState {
            playing: self.shared.playing.load(Ordering::Relaxed),
            position: self.shared.pos_frames.load(Ordering::Relaxed) as f64 / rate,
            duration: self.shared.duration_ms.load(Ordering::Relaxed) as f64 / 1000.0,
            track_id: self.shared.current_track.load(Ordering::Relaxed),
            ended: self.shared.ended.load(Ordering::Relaxed),
            sample_rate: self.shared.src_rate.load(Ordering::Relaxed),
            bit_depth: self.shared.src_bits.load(Ordering::Relaxed),
            codec: self
                .shared
                .codec
                .lock()
                .map(|c| c.clone())
                .unwrap_or_default(),
            error: self.shared.last_error.lock().ok().and_then(|e| e.clone()),
        }
    }
}

/* ------------------------------- output ------------------------------- */

/// Everything the output callback needs, kept out of the closure so the same
/// logic can serve every sample format the host might ask for — and be tested
/// without an audio device.
struct Mixer {
    audio_rx: rtrb::Consumer<f32>,
    marker_rx: rtrb::Consumer<TrackMarker>,
    fft_tx: rtrb::Producer<f32>,
    shared: Arc<Shared>,
    equalizer: Equalizer,
    channels: usize,
    eq_seen: u64,
    flush_seen: u64,
    origin_global: u64,
    origin_pos: u64,
    pending: Option<TrackMarker>,
}

impl Mixer {
    /// Fill one buffer of interleaved f32 frames. Allocation-free and
    /// non-blocking: an empty ring simply yields silence.
    fn fill(&mut self, out: &mut [f32]) {
        let sh = Arc::clone(&self.shared);
        let version = sh.eq_version.load(Ordering::Acquire);
        if version != self.eq_seen {
            let (gains, preamp, enabled) = sh.eq_params();
            self.equalizer.set_params(&gains, preamp, enabled);
            self.eq_seen = version;
        }

        // A seek or track load invalidates everything still queued.
        let flush = sh.flush_version.load(Ordering::Acquire);
        if flush != self.flush_seen {
            let stale = self.audio_rx.slots();
            if stale > 0 {
                if let Ok(chunk) = self.audio_rx.read_chunk(stale) {
                    chunk.commit_all();
                }
            }
            self.equalizer.reset(); // filter memory belongs to the old audio
            self.flush_seen = flush;
        }

        let channels = self.channels;
        let mut filled = 0usize;
        if sh.playing.load(Ordering::Relaxed) {
            let want = out.len().min(self.audio_rx.slots());
            if want >= channels {
                if let Ok(chunk) = self.audio_rx.read_chunk(want) {
                    let (a, b) = chunk.as_slices();
                    let usable = ((a.len() + b.len()) / channels) * channels; // whole frames only
                    let from_a = a.len().min(usable);
                    out[..from_a].copy_from_slice(&a[..from_a]);
                    if usable > from_a {
                        out[from_a..usable].copy_from_slice(&b[..usable - from_a]);
                    }
                    filled = usable;
                    chunk.commit(usable);
                }
            }
        }
        // Underrun or paused: silence the remainder.
        out[filled..].fill(0.0);

        let frames_out = filled / channels;
        if frames_out == 0 {
            return;
        }

        self.equalizer
            .process_interleaved(&mut out[..filled], channels);
        let vol = sh.volume();
        if (vol - 1.0).abs() > f32::EPSILON {
            for s in out[..filled].iter_mut() {
                *s *= vol;
            }
        }

        let played = sh
            .played_frames
            .fetch_add(frames_out as u64, Ordering::Relaxed)
            + frames_out as u64;

        // Advance across any track boundary we just crossed.
        loop {
            if self.pending.is_none() {
                self.pending = self.marker_rx.pop().ok();
            }
            match self.pending {
                Some(m) if played >= m.at_global_frame => {
                    self.origin_global = m.at_global_frame;
                    self.origin_pos = m.track_start_frame;
                    sh.current_track.store(m.track_id, Ordering::Relaxed);
                    self.pending = None;
                }
                _ => break,
            }
        }
        sh.pos_frames.store(
            self.origin_pos + played.saturating_sub(self.origin_global),
            Ordering::Relaxed,
        );

        // Mono mix for the spectrum; dropping samples when the analyser is
        // behind is fine, it only needs recent audio.
        for frame in out[..filled].chunks(channels) {
            let mono = frame.iter().sum::<f32>() / channels as f32;
            let _ = self.fft_tx.push(mono);
        }
    }
}

/// Build the stream for a concrete sample type. Hosts differ: WASAPI hands out
/// f32, while ALSA and PulseAudio commonly want i16 — assuming f32 everywhere
/// makes cpal panic on those hosts.
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut mixer: Mixer,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let mut scratch: Vec<f32> = Vec::new();
    device.build_output_stream(
        config,
        move |out: &mut [T], _| {
            if scratch.len() != out.len() {
                scratch.resize(out.len(), 0.0);
            }
            mixer.fill(&mut scratch);
            for (dst, src) in out.iter_mut().zip(scratch.iter()) {
                *dst = T::from_sample(*src);
            }
        },
        move |err| {
            if let Ok(mut slot) = shared.last_error.lock() {
                *slot = Some(format!("audio output error: {err}"));
            }
        },
        None,
    )
}

fn spawn_output_thread(
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
    audio_rx: rtrb::Consumer<f32>,
    marker_rx: rtrb::Consumer<TrackMarker>,
    fft_tx: rtrb::Producer<f32>,
    shared: Arc<Shared>,
) {
    std::thread::Builder::new()
        .name("horri-audio-out".into())
        .spawn(move || {
            let channels = config.channels() as usize;
            let rate = config.sample_rate();
            let mixer = Mixer {
                audio_rx,
                marker_rx,
                fft_tx,
                shared: Arc::clone(&shared),
                equalizer: Equalizer::new(rate, channels),
                channels,
                eq_seen: u64::MAX,
                flush_seen: 0,
                origin_global: 0,
                origin_pos: 0,
                pending: None,
            };

            let cfg = config.config();
            let stream = match config.sample_format() {
                cpal::SampleFormat::F32 => {
                    build_stream::<f32>(&device, &cfg, mixer, Arc::clone(&shared))
                }
                cpal::SampleFormat::I16 => {
                    build_stream::<i16>(&device, &cfg, mixer, Arc::clone(&shared))
                }
                cpal::SampleFormat::U16 => {
                    build_stream::<u16>(&device, &cfg, mixer, Arc::clone(&shared))
                }
                cpal::SampleFormat::I32 => {
                    build_stream::<i32>(&device, &cfg, mixer, Arc::clone(&shared))
                }
                cpal::SampleFormat::U8 => {
                    build_stream::<u8>(&device, &cfg, mixer, Arc::clone(&shared))
                }
                cpal::SampleFormat::F64 => {
                    build_stream::<f64>(&device, &cfg, mixer, Arc::clone(&shared))
                }
                other => {
                    if let Ok(mut slot) = shared.last_error.lock() {
                        *slot = Some(format!("unsupported audio sample format: {other:?}"));
                    }
                    return;
                }
            };

            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    if let Ok(mut slot) = shared.last_error.lock() {
                        *slot = Some(format!("cannot open audio device: {e}"));
                    }
                    return;
                }
            };
            if let Err(e) = stream.play() {
                if let Ok(mut slot) = shared.last_error.lock() {
                    *slot = Some(format!("cannot start audio stream: {e}"));
                }
                return;
            }
            // The stream must outlive this scope; park the thread to keep it alive.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        })
        .expect("spawn audio output thread");
}

/* ------------------------------- decoder ------------------------------- */

struct Playing {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    src_rate: u32,
    resampler: Option<SincFixedIn<f32>>,
    /// Planar staging buffers feeding the resampler.
    stage: Vec<Vec<f32>>,
    id: u64,
}

fn spawn_decoder_thread(
    cmd_rx: mpsc::Receiver<Cmd>,
    mut audio_tx: rtrb::Producer<f32>,
    mut marker_tx: rtrb::Producer<TrackMarker>,
    shared: Arc<Shared>,
    out_rate: u32,
    out_channels: usize,
) {
    std::thread::Builder::new()
        .name("horri-audio-dec".into())
        .spawn(move || {
            let mut current: Option<Playing> = None;
            let mut next_up: Option<(Source, u64)> = None;
            // Frames pushed into the ring since start-up; markers are placed on
            // this timeline so the callback can map frames back to tracks.
            let mut written: u64 = 0;

            loop {
                // Drain commands first so transport controls stay responsive.
                let blocking = current.is_none() || !shared.playing.load(Ordering::Relaxed);
                let cmd = if blocking {
                    cmd_rx.recv_timeout(Duration::from_millis(100)).ok()
                } else {
                    cmd_rx.try_recv().ok()
                };

                if let Some(cmd) = cmd {
                    match cmd {
                        Cmd::Load {
                            source,
                            track_id,
                            autoplay,
                        } => {
                            shared.playing.store(false, Ordering::Relaxed);
                            drain(&shared, &mut audio_tx);
                            reset_position(&shared, &mut written, &mut marker_tx, track_id, 0);
                            match open_source(&source, out_rate, out_channels, &shared) {
                                Ok(mut p) => {
                                    p.id = track_id;
                                    current = Some(p);
                                    shared.ended.store(false, Ordering::Relaxed);
                                    shared.current_track.store(track_id, Ordering::Relaxed);
                                    if autoplay {
                                        shared.playing.store(true, Ordering::Relaxed);
                                    }
                                }
                                Err(e) => {
                                    current = None;
                                    set_error(&shared, Some(e));
                                    shared.ended.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                        Cmd::SetNext(n) => next_up = n,
                        Cmd::Play => {
                            if current.is_some() {
                                shared.playing.store(true, Ordering::Relaxed);
                            }
                        }
                        Cmd::Pause => shared.playing.store(false, Ordering::Relaxed),
                        Cmd::Stop => {
                            shared.playing.store(false, Ordering::Relaxed);
                            current = None;
                            drain(&shared, &mut audio_tx);
                        }
                        Cmd::Seek(secs) => {
                            if let Some(p) = current.as_mut() {
                                let seeked = p.reader.seek(
                                    SeekMode::Accurate,
                                    SeekTo::Time {
                                        time: Time::from(secs),
                                        track_id: Some(p.track_id),
                                    },
                                );
                                p.decoder.reset();
                                if let Some(r) = p.resampler.as_mut() {
                                    r.reset();
                                }
                                for c in p.stage.iter_mut() {
                                    c.clear();
                                }
                                drain(&shared, &mut audio_tx);
                                // Trust where the demuxer actually landed.
                                let landed = match seeked {
                                    Ok(to) => to.actual_ts as f64 / p.src_rate.max(1) as f64,
                                    Err(_) => secs,
                                };
                                let target = (landed * out_rate as f64) as u64;
                                let id = p.id;
                                reset_position(&shared, &mut written, &mut marker_tx, id, target);
                            }
                        }
                    }
                    continue;
                }

                if !shared.playing.load(Ordering::Relaxed) {
                    continue;
                }

                let Some(p) = current.as_mut() else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };

                // Keep the ring topped up, but leave room so a push never blocks.
                if audio_tx.slots() < RESAMPLE_CHUNK * out_channels * 2 {
                    std::thread::sleep(Duration::from_millis(4));
                    continue;
                }

                match decode_and_push(p, &mut audio_tx, out_channels, &mut written) {
                    DecodeStep::Continued => {}
                    DecodeStep::Failed(e) => {
                        set_error(&shared, Some(e));
                        current = None;
                        shared.ended.store(true, Ordering::Relaxed);
                        shared.playing.store(false, Ordering::Relaxed);
                    }
                    DecodeStep::Finished => {
                        // Gapless: start the queued track without draining the ring.
                        if let Some((src, id)) = next_up.take() {
                            match open_source(&src, out_rate, out_channels, &shared) {
                                Ok(mut np) => {
                                    np.id = id;
                                    let _ = marker_tx.push(TrackMarker {
                                        at_global_frame: written,
                                        track_id: id,
                                        track_start_frame: 0,
                                    });
                                    current = Some(np);
                                }
                                Err(e) => {
                                    set_error(&shared, Some(e));
                                    current = None;
                                    shared.ended.store(true, Ordering::Relaxed);
                                    shared.playing.store(false, Ordering::Relaxed);
                                }
                            }
                        } else {
                            current = None;
                            shared.ended.store(true, Ordering::Relaxed);
                            shared.playing.store(false, Ordering::Relaxed);
                        }
                    }
                }
            }
        })
        .expect("spawn audio decoder thread");
}

enum DecodeStep {
    Continued,
    Finished,
    Failed(String),
}

fn decode_and_push(
    p: &mut Playing,
    audio_tx: &mut rtrb::Producer<f32>,
    out_channels: usize,
    written: &mut u64,
) -> DecodeStep {
    let packet = match p.reader.next_packet() {
        Ok(pkt) => pkt,
        Err(symphonia::core::errors::Error::IoError(e))
            if e.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            return DecodeStep::Finished
        }
        Err(symphonia::core::errors::Error::ResetRequired) => return DecodeStep::Finished,
        Err(e) => return DecodeStep::Failed(format!("read error: {e}")),
    };
    if packet.track_id() != p.track_id {
        return DecodeStep::Continued;
    }

    let decoded = match p.decoder.decode(&packet) {
        Ok(d) => d,
        // Recoverable glitches: skip the packet rather than kill playback.
        Err(symphonia::core::errors::Error::DecodeError(_)) => return DecodeStep::Continued,
        Err(e) => return DecodeStep::Failed(format!("decode error: {e}")),
    };

    let spec = *decoded.spec();
    let frames = decoded.frames();
    if frames == 0 {
        return DecodeStep::Continued;
    }
    let src_channels = spec.channels.count();
    let mut sample_buf = SampleBuffer::<f32>::new(frames as u64, spec);
    sample_buf.copy_interleaved_ref(decoded);
    let samples = sample_buf.samples();

    match p.resampler.as_mut() {
        None => {
            // Fast path: device rate matches the file, push straight through.
            let mut out = Vec::with_capacity(frames * out_channels);
            for f in 0..frames {
                let frame = &samples[f * src_channels..(f + 1) * src_channels];
                write_frame(&mut out, frame, out_channels);
            }
            push_all(audio_tx, &out, out_channels, written);
        }
        Some(resampler) => {
            for f in 0..frames {
                let frame = &samples[f * src_channels..(f + 1) * src_channels];
                for ch in 0..p.stage.len() {
                    // stage is laid out in source channels
                    let v = frame.get(ch).copied().unwrap_or(0.0);
                    p.stage[ch].push(v);
                }
                if p.stage[0].len() >= RESAMPLE_CHUNK {
                    let input: Vec<Vec<f32>> = p
                        .stage
                        .iter_mut()
                        .map(|c| c.drain(..RESAMPLE_CHUNK).collect())
                        .collect();
                    match resampler.process(&input, None) {
                        Ok(res) => {
                            let n = res.first().map(|c| c.len()).unwrap_or(0);
                            let mut out = Vec::with_capacity(n * out_channels);
                            let mut frame_buf = vec![0.0f32; res.len()];
                            for i in 0..n {
                                for (ch, chan) in res.iter().enumerate() {
                                    frame_buf[ch] = chan[i];
                                }
                                write_frame(&mut out, &frame_buf, out_channels);
                            }
                            push_all(audio_tx, &out, out_channels, written);
                        }
                        Err(e) => return DecodeStep::Failed(format!("resample error: {e}")),
                    }
                }
            }
        }
    }
    DecodeStep::Continued
}

/// Map a decoded source frame onto the device's channel layout.
#[inline]
fn write_frame(out: &mut Vec<f32>, frame: &[f32], out_channels: usize) {
    match (frame.len(), out_channels) {
        (0, _) => out.extend(std::iter::repeat(0.0).take(out_channels)),
        (1, n) => out.extend(std::iter::repeat(frame[0]).take(n)), // mono → all
        (_, n) => {
            for ch in 0..n {
                // Duplicate the last available channel rather than dropping to
                // silence when the file has fewer channels than the device.
                let v = frame
                    .get(ch)
                    .copied()
                    .unwrap_or_else(|| frame.last().copied().unwrap_or(0.0));
                out.push(v);
            }
        }
    }
}

fn push_all(
    audio_tx: &mut rtrb::Producer<f32>,
    data: &[f32],
    out_channels: usize,
    written: &mut u64,
) {
    let mut pushed = 0usize;
    while pushed < data.len() {
        let space = audio_tx.slots();
        if space == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        let n = space.min(data.len() - pushed);
        if let Ok(chunk) = audio_tx.write_chunk_uninit(n) {
            chunk.fill_from_iter(data[pushed..pushed + n].iter().copied());
            pushed += n;
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    *written += (data.len() / out_channels.max(1)) as u64;
}

/// Ask the callback to discard queued audio and wait until it has, so the
/// decoder's frame counter and the callback's stay on the same timeline.
fn drain(shared: &Arc<Shared>, audio_tx: &mut rtrb::Producer<f32>) {
    shared.flush_version.fetch_add(1, Ordering::Release);
    let capacity = audio_tx.buffer().capacity();
    for _ in 0..50 {
        if audio_tx.slots() >= capacity {
            return; // ring empty again
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Re-anchor the timeline: the next audio pushed starts `start_frame` into
/// `track_id`, played from the current global frame onwards.
fn reset_position(
    shared: &Arc<Shared>,
    written: &mut u64,
    marker_tx: &mut rtrb::Producer<TrackMarker>,
    track_id: u64,
    start_frame: u64,
) {
    let played = shared.played_frames.load(Ordering::Relaxed);
    *written = played;
    shared.pos_frames.store(start_frame, Ordering::Relaxed);
    let _ = marker_tx.push(TrackMarker {
        at_global_frame: played,
        track_id,
        track_start_frame: start_frame,
    });
}

fn set_error(shared: &Arc<Shared>, e: Option<String>) {
    if let Ok(mut slot) = shared.last_error.lock() {
        *slot = e;
    }
}

fn open_source(
    source: &Source,
    out_rate: u32,
    out_channels: usize,
    shared: &Arc<Shared>,
) -> Result<Playing, String> {
    let mss = match source {
        Source::File(path) => {
            let file = File::open(path).map_err(|e| format!("cannot open file: {e}"))?;
            MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default())
        }
        Source::Url(url) => {
            let src =
                HttpSource::new(url.clone()).map_err(|e| format!("cannot open stream: {e}"))?;
            MediaSourceStream::new(Box::new(src), MediaSourceStreamOptions::default())
        }
    };

    let probed = symphonia::default::get_probe()
        .format(
            &source.hint(),
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("unsupported or corrupt audio: {e}"))?;

    let reader = probed.format;
    let track = reader
        .default_track()
        .ok_or_else(|| "file contains no audio track".to_string())?;
    let track_id = track.id;
    let params = track.codec_params.clone();

    let decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("no decoder for this format: {e}"))?;

    let src_rate = params.sample_rate.unwrap_or(out_rate);
    let src_channels = params
        .channels
        .map(|c| c.count())
        .unwrap_or(out_channels)
        .max(1);

    // Duration, when the container knows it.
    let duration_ms = params
        .n_frames
        .and_then(|frames| {
            params
                .sample_rate
                .map(|sr| (frames as f64 / sr as f64 * 1000.0) as u64)
        })
        .unwrap_or(0);
    shared.duration_ms.store(duration_ms, Ordering::Relaxed);
    shared.src_rate.store(src_rate, Ordering::Relaxed);
    shared.src_bits.store(
        params
            .bits_per_sample
            .or(params.bits_per_coded_sample)
            .unwrap_or(0),
        Ordering::Relaxed,
    );
    if let Ok(mut slot) = shared.codec.lock() {
        *slot = codec_name(params.codec);
    }
    set_error(shared, None);

    let resampler = if src_rate == out_rate {
        None
    } else {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        Some(
            SincFixedIn::<f32>::new(
                out_rate as f64 / src_rate as f64,
                1.1,
                params,
                RESAMPLE_CHUNK,
                src_channels,
            )
            .map_err(|e| format!("cannot build resampler: {e}"))?,
        )
    };

    Ok(Playing {
        reader,
        decoder,
        track_id,
        src_rate,
        resampler,
        stage: vec![Vec::with_capacity(RESAMPLE_CHUNK * 2); src_channels],
        id: 0,
    })
}

/// True for formats that reproduce the original samples exactly.
fn codec_is_lossless(codec: symphonia::core::codecs::CodecType) -> bool {
    use symphonia::core::codecs::*;
    matches!(
        codec,
        CODEC_TYPE_FLAC
            | CODEC_TYPE_ALAC
            | CODEC_TYPE_PCM_S16LE
            | CODEC_TYPE_PCM_S24LE
            | CODEC_TYPE_PCM_S32LE
            | CODEC_TYPE_PCM_F32LE
            | CODEC_TYPE_PCM_F64LE
            | CODEC_TYPE_PCM_S16BE
            | CODEC_TYPE_PCM_S24BE
            | CODEC_TYPE_PCM_S32BE
            | CODEC_TYPE_PCM_U8
            | CODEC_TYPE_PCM_S8
    )
}

fn codec_name(codec: symphonia::core::codecs::CodecType) -> String {
    use symphonia::core::codecs::*;
    match codec {
        CODEC_TYPE_FLAC => "FLAC",
        CODEC_TYPE_ALAC => "ALAC",
        CODEC_TYPE_MP3 => "MP3",
        CODEC_TYPE_AAC => "AAC",
        CODEC_TYPE_VORBIS => "Vorbis",
        CODEC_TYPE_PCM_S16LE | CODEC_TYPE_PCM_S24LE | CODEC_TYPE_PCM_S32LE
        | CODEC_TYPE_PCM_F32LE | CODEC_TYPE_PCM_S16BE | CODEC_TYPE_PCM_S24BE
        | CODEC_TYPE_PCM_S32BE => "PCM",
        CODEC_TYPE_ADPCM_IMA_WAV | CODEC_TYPE_ADPCM_MS => "ADPCM",
        _ => "audio",
    }
    .to_string()
}

/* ------------------------------- monitor ------------------------------- */

fn spawn_monitor_thread(app: AppHandle, mut fft_rx: rtrb::Consumer<f32>, shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("horri-audio-mon".into())
        .spawn(move || {
            let mut planner = RealFftPlanner::<f32>::new();
            let fft = planner.plan_fft_forward(FFT_SIZE);
            let mut scratch = fft.make_scratch_vec();
            let mut spectrum_out = fft.make_output_vec();
            let mut window = vec![0.0f32; FFT_SIZE];
            for (i, w) in window.iter_mut().enumerate() {
                // Hann window
                let x = i as f32 / (FFT_SIZE - 1) as f32;
                *w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos();
            }
            let mut sample_ring = vec![0.0f32; FFT_SIZE];
            let mut smooth = [0.0f32; SPECTRUM_BARS];
            let mut last_track = 0u64;
            let mut last_emit_idle = std::time::Instant::now();

            loop {
                std::thread::sleep(Duration::from_millis(33));

                // Pull whatever the callback produced; keep only the newest window.
                let available = fft_rx.slots();
                if available > 0 {
                    if let Ok(chunk) = fft_rx.read_chunk(available) {
                        let (a, b) = chunk.as_slices();
                        let joined = a.iter().chain(b.iter()).copied().collect::<Vec<f32>>();
                        let take = joined.len().min(FFT_SIZE);
                        sample_ring.rotate_left(take);
                        let start = FFT_SIZE - take;
                        sample_ring[start..].copy_from_slice(&joined[joined.len() - take..]);
                        chunk.commit_all();
                    }
                }

                let playing = shared.playing.load(Ordering::Relaxed);
                let bars = if playing {
                    let mut input: Vec<f32> = sample_ring
                        .iter()
                        .zip(window.iter())
                        .map(|(s, w)| s * w)
                        .collect();
                    if fft
                        .process_with_scratch(&mut input, &mut spectrum_out, &mut scratch)
                        .is_ok()
                    {
                        bucket_spectrum(&spectrum_out, &mut smooth)
                    } else {
                        decay(&mut smooth)
                    }
                } else {
                    decay(&mut smooth)
                };

                if let Ok(mut slot) = shared.spectrum.lock() {
                    *slot = smooth;
                }

                let track = shared.current_track.load(Ordering::Relaxed);
                let ended = shared.ended.load(Ordering::Relaxed);
                let changed = track != last_track;
                last_track = track;

                // While idle, emit rarely — just enough to keep the UI in sync.
                if !playing && !changed && !ended {
                    if last_emit_idle.elapsed() < Duration::from_millis(500) {
                        continue;
                    }
                    last_emit_idle = std::time::Instant::now();
                }

                let rate = shared.out_rate.load(Ordering::Relaxed).max(1) as f64;
                let tick = Tick {
                    playing,
                    position: shared.pos_frames.load(Ordering::Relaxed) as f64 / rate,
                    duration: shared.duration_ms.load(Ordering::Relaxed) as f64 / 1000.0,
                    track_id: track,
                    ended,
                    bars: bars
                        .iter()
                        .map(|v| (v * 255.0).clamp(0.0, 255.0) as u8)
                        .collect(),
                    sample_rate: shared.src_rate.load(Ordering::Relaxed),
                    bit_depth: shared.src_bits.load(Ordering::Relaxed),
                    codec: shared.codec.lock().map(|c| c.clone()).unwrap_or_default(),
                };
                let _ = app.emit("audio:tick", tick);
            }
        })
        .expect("spawn audio monitor thread");
}

fn decay(smooth: &mut [f32; SPECTRUM_BARS]) -> [f32; SPECTRUM_BARS] {
    for v in smooth.iter_mut() {
        *v *= 0.85;
    }
    *smooth
}

/// Group FFT bins into logarithmically spaced bars, matching how the UI drew
/// them before, so the visualiser looks the same as the WebAudio version.
fn bucket_spectrum(
    bins: &[realfft::num_complex::Complex<f32>],
    smooth: &mut [f32; SPECTRUM_BARS],
) -> [f32; SPECTRUM_BARS] {
    let n = bins.len();
    for (i, slot) in smooth.iter_mut().enumerate() {
        let lo = ((i as f32 / SPECTRUM_BARS as f32).powf(1.4) * n as f32) as usize;
        let hi = (((i + 1) as f32 / SPECTRUM_BARS as f32).powf(1.4) * n as f32) as usize;
        let hi = hi.max(lo + 1).min(n);
        let peak = bins[lo.min(n.saturating_sub(1))..hi]
            .iter()
            .fold(0.0f32, |acc, bin| acc.max(bin.norm()));
        // Normalise and shape to roughly match the old byte-frequency curve.
        let v = (peak / (FFT_SIZE as f32 * 0.25)).sqrt().clamp(0.0, 1.0);
        *slot = *slot * 0.72 + v * 0.28;
    }
    *smooth
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources() {
        assert!(matches!(
            Source::parse("https://example.com/a.flac"),
            Source::Url(_)
        ));
        assert!(matches!(Source::parse(r"C:\music\a.flac"), Source::File(_)));
    }

    #[test]
    fn hints_extension_for_files_and_urls() {
        // Extension hints let Symphonia pick a demuxer without sniffing.
        let f = Source::parse("/music/song.flac");
        let u = Source::parse("https://host/rest/stream.m4a?id=7&u=admin");
        // Hint has no public getter; constructing it must simply not panic.
        let _ = f.hint();
        let _ = u.hint();
    }

    #[test]
    fn mono_frames_expand_to_every_output_channel() {
        let mut out = Vec::new();
        write_frame(&mut out, &[0.5], 2);
        assert_eq!(out, vec![0.5, 0.5]);
    }

    #[test]
    fn stereo_frames_pass_through_unchanged() {
        let mut out = Vec::new();
        write_frame(&mut out, &[0.25, -0.25], 2);
        assert_eq!(out, vec![0.25, -0.25]);
    }

    #[test]
    fn surround_sources_are_truncated_to_device_channels() {
        let mut out = Vec::new();
        write_frame(&mut out, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 2);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    /// Build a Mixer wired to rings we control, with no audio device involved.
    fn test_mixer(
        channels: usize,
    ) -> (
        Mixer,
        rtrb::Producer<f32>,
        rtrb::Producer<TrackMarker>,
        Arc<Shared>,
    ) {
        let (audio_tx, audio_rx) = rtrb::RingBuffer::<f32>::new(4096);
        let (marker_tx, marker_rx) = rtrb::RingBuffer::<TrackMarker>::new(16);
        let (fft_tx, _fft_rx) = rtrb::RingBuffer::<f32>::new(4096);
        let shared = Arc::new(Shared::new());
        let mixer = Mixer {
            audio_rx,
            marker_rx,
            fft_tx,
            shared: Arc::clone(&shared),
            equalizer: Equalizer::new(48_000, channels),
            channels,
            eq_seen: u64::MAX,
            flush_seen: 0,
            origin_global: 0,
            origin_pos: 0,
            pending: None,
        };
        (mixer, audio_tx, marker_tx, shared)
    }

    fn push(tx: &mut rtrb::Producer<f32>, data: &[f32]) {
        for v in data {
            tx.push(*v).expect("ring has room");
        }
    }

    #[test]
    fn silence_is_output_when_paused() {
        let (mut mixer, mut tx, _mk, shared) = test_mixer(2);
        push(&mut tx, &[0.5; 64]);
        shared.playing.store(false, Ordering::Relaxed);
        let mut out = [1.0f32; 64];
        mixer.fill(&mut out);
        assert!(
            out.iter().all(|v| *v == 0.0),
            "paused output must be silent"
        );
        assert_eq!(
            shared.played_frames.load(Ordering::Relaxed),
            0,
            "paused playback must not advance position"
        );
    }

    #[test]
    fn underrun_fills_the_rest_with_silence_instead_of_repeating() {
        let (mut mixer, mut tx, _mk, shared) = test_mixer(2);
        shared.playing.store(true, Ordering::Relaxed);
        push(&mut tx, &[0.25; 8]); // only 4 frames for a 16-frame buffer
        let mut out = [9.0f32; 32];
        mixer.fill(&mut out);
        assert!(out[..8].iter().all(|v| (*v - 0.25).abs() < 1e-6));
        assert!(
            out[8..].iter().all(|v| *v == 0.0),
            "missing audio must become silence, not stale samples"
        );
    }

    #[test]
    fn position_follows_playback_and_track_markers() {
        let (mut mixer, mut tx, mut mk, shared) = test_mixer(2);
        shared.playing.store(true, Ordering::Relaxed);

        // First track starts at global frame 0.
        mk.push(TrackMarker {
            at_global_frame: 0,
            track_id: 7,
            track_start_frame: 0,
        })
        .unwrap();
        push(&mut tx, &[0.1; 64]); // 32 frames
        let mut out = [0.0f32; 64];
        mixer.fill(&mut out);
        assert_eq!(shared.pos_frames.load(Ordering::Relaxed), 32);
        assert_eq!(shared.current_track.load(Ordering::Relaxed), 7);

        // Gapless hand-off: next track begins at global frame 32.
        mk.push(TrackMarker {
            at_global_frame: 32,
            track_id: 8,
            track_start_frame: 0,
        })
        .unwrap();
        push(&mut tx, &[0.1; 64]);
        mixer.fill(&mut out);
        assert_eq!(shared.current_track.load(Ordering::Relaxed), 8);
        assert_eq!(
            shared.pos_frames.load(Ordering::Relaxed),
            32,
            "position must restart within the new track, not keep counting"
        );
    }

    #[test]
    fn seek_marker_reports_position_inside_the_track() {
        let (mut mixer, mut tx, mut mk, shared) = test_mixer(2);
        shared.playing.store(true, Ordering::Relaxed);
        // Seeked to frame 1000; playback resumes at global frame 0.
        mk.push(TrackMarker {
            at_global_frame: 0,
            track_id: 3,
            track_start_frame: 1000,
        })
        .unwrap();
        push(&mut tx, &[0.2; 40]); // 20 frames
        let mut out = [0.0f32; 40];
        mixer.fill(&mut out);
        assert_eq!(shared.pos_frames.load(Ordering::Relaxed), 1020);
    }

    #[test]
    fn flush_discards_queued_audio() {
        let (mut mixer, mut tx, _mk, shared) = test_mixer(2);
        shared.playing.store(true, Ordering::Relaxed);
        push(&mut tx, &[0.75; 64]);
        // A seek happened before this buffer was ever played.
        shared.flush_version.fetch_add(1, Ordering::Release);
        let mut out = [0.0f32; 64];
        mixer.fill(&mut out);
        assert!(
            out.iter().all(|v| *v == 0.0),
            "audio queued before a seek must never reach the device"
        );
    }

    #[test]
    fn volume_scales_the_output() {
        let (mut mixer, mut tx, _mk, shared) = test_mixer(2);
        shared.playing.store(true, Ordering::Relaxed);
        shared
            .volume_bits
            .store(0.5f32.to_bits(), Ordering::Relaxed);
        push(&mut tx, &[1.0; 16]);
        let mut out = [0.0f32; 16];
        mixer.fill(&mut out);
        assert!(out.iter().all(|v| (*v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn partial_frames_are_never_split_across_buffers() {
        // 3 samples with 2 channels is one whole frame plus a stray sample; the
        // stray must stay in the ring so channels don't swap places.
        let (mut mixer, mut tx, _mk, shared) = test_mixer(2);
        shared.playing.store(true, Ordering::Relaxed);
        push(&mut tx, &[1.0, -1.0, 1.0]);
        let mut out = [0.0f32; 8];
        mixer.fill(&mut out);
        assert_eq!(shared.played_frames.load(Ordering::Relaxed), 1);
        assert_eq!(&out[..2], &[1.0, -1.0]);
    }

    #[test]
    fn spectrum_bars_stay_in_range() {
        let bins: Vec<realfft::num_complex::Complex<f32>> = (0..513)
            .map(|i| realfft::num_complex::Complex::new(i as f32, 0.0))
            .collect();
        let mut smooth = [0.0f32; SPECTRUM_BARS];
        for _ in 0..50 {
            bucket_spectrum(&bins, &mut smooth);
        }
        assert!(smooth.iter().all(|v| (0.0..=1.0).contains(v)));
    }
}
