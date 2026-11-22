//! Voice notes: Ogg/Opus decoding + playback and capture + encoding.
//!
//! Everything that touches an audio device runs on a dedicated thread
//! (cpal streams are not `Send`); the UI only sends commands and receives
//! events through callbacks.

use std::fs::File;
use std::io::{BufWriter, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};

/// Opus always runs at 48 kHz inside Ogg (RFC 7845).
const RATE: u32 = 48_000;
/// 20 ms frames.
const FRAME: usize = 960;
const BITRATE: i32 = 28_000;
const MAX_PACKET: usize = 4000;
/// Longest Opus packet is 120 ms.
const MAX_DECODE_FRAMES: usize = 5760;
/// Decoded audio is kept in memory, so refuse absurdly long files.
const MAX_PLAY_SECS: usize = 20 * 60;
/// Refuse to read bigger files into memory for playback.
const MAX_PLAY_BYTES: u64 = 32 * 1024 * 1024;
/// Force an Ogg page boundary roughly every second.
const PAGE_EVERY_FRAMES: u64 = 50;
const TICK: Duration = Duration::from_millis(100);

/// Playback speeds a voice note cycles through.
const SPEEDS: [f32; 3] = [1.0, 1.5, 2.0];
/// Index into `SPEEDS`; one global value for the whole session.
static SPEED_IDX: AtomicUsize = AtomicUsize::new(0);

/// The speed after `idx` in the 1x -> 1.5x -> 2x -> 1x cycle.
fn next_speed_idx(idx: usize) -> usize {
    (idx + 1) % SPEEDS.len()
}

fn speed_at(idx: usize) -> f32 {
    SPEEDS[idx.min(SPEEDS.len() - 1)]
}

pub fn speed_label_at(idx: usize) -> &'static str {
    match idx.min(SPEEDS.len() - 1) {
        0 => "1\u{00d7}",
        1 => "1.5\u{00d7}",
        _ => "2\u{00d7}",
    }
}

/// Advances the session-wide voice speed and returns its new index.
pub fn cycle_speed() -> usize {
    let next = next_speed_idx(SPEED_IDX.load(Ordering::Relaxed));
    SPEED_IDX.store(next, Ordering::Relaxed);
    next
}

/// Source frames consumed per output frame. Speeding up simply reads the
/// decoded samples faster (resampling), so the pitch rises with the speed.
fn playback_step(src_rate: u32, out_rate: u32, speed: f32) -> f64 {
    f64::from(src_rate) / f64::from(out_rate) * f64::from(speed)
}

// ---- encoding ----------------------------------------------------------------

/// Mono 48 kHz f32 -> Ogg/Opus. Independent of any device so it can be fed
/// from the recorder, the demo data or tests.
pub struct VoiceEncoder<W: Write> {
    writer: PacketWriter<'static, W>,
    enc: Encoder,
    serial: u32,
    pre_skip: u64,
    pending: Vec<f32>,
    total_in: u64,
    frames: u64,
    /// The newest packet is held back so the last one can close the stream.
    held: Option<(Vec<u8>, u64)>,
    out: Vec<u8>,
}

impl<W: Write> VoiceEncoder<W> {
    pub fn new(w: W) -> Result<Self> {
        let mut enc = Encoder::new(RATE, Channels::Mono, Application::Voip).context("create Opus encoder")?;
        enc.set_bitrate(Bitrate::Bits(BITRATE)).context("set Opus bitrate")?;
        let pre_skip = u64::try_from(enc.get_lookahead().context("query Opus lookahead")?)
            .context("negative Opus lookahead")?;
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(1);
        let mut writer = PacketWriter::new(w);

        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(1); // channels
        head.extend_from_slice(&(pre_skip as u16).to_le_bytes());
        head.extend_from_slice(&RATE.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family
        writer
            .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
            .context("write OpusHead")?;

        let vendor = b"waclient";
        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments
        writer
            .write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
            .context("write OpusTags")?;

        Ok(Self {
            writer,
            enc,
            serial,
            pre_skip,
            pending: Vec::with_capacity(FRAME * 2),
            total_in: 0,
            frames: 0,
            held: None,
            out: vec![0; MAX_PACKET],
        })
    }

    /// Samples pushed so far (mono, 48 kHz).
    pub fn samples(&self) -> u64 {
        self.total_in
    }

    pub fn push(&mut self, samples: &[f32]) -> Result<()> {
        self.total_in += samples.len() as u64;
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= FRAME {
            let frame: Vec<f32> = self.pending.drain(..FRAME).collect();
            self.encode_frame(&frame)?;
        }
        Ok(())
    }

    fn encode_frame(&mut self, frame: &[f32]) -> Result<()> {
        let n = self.enc.encode_float(frame, &mut self.out).context("Opus encode")?;
        self.frames += 1;
        let packet = self.out[..n].to_vec();
        self.emit(packet, self.frames * FRAME as u64)
    }

    fn emit(&mut self, packet: Vec<u8>, granule: u64) -> Result<()> {
        if let Some((prev, prev_granule)) = self.held.replace((packet, granule)) {
            let info = if (prev_granule / FRAME as u64) % PAGE_EVERY_FRAMES == 0 {
                PacketWriteEndInfo::EndPage
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            self.writer
                .write_packet(prev, self.serial, info, prev_granule)
                .context("write Ogg packet")?;
        }
        Ok(())
    }

    /// Flushes the tail, closes the stream and returns the writer plus the
    /// duration in seconds of the audio that was pushed.
    pub fn finish(mut self) -> Result<(W, f32)> {
        if !self.pending.is_empty() {
            let mut frame = std::mem::take(&mut self.pending);
            frame.resize(FRAME, 0.0);
            self.encode_frame(&frame)?;
        }
        // The decoder drops `pre_skip` samples first, so enough frames must
        // exist to cover pre_skip + everything that was pushed.
        let target = self.pre_skip + self.total_in;
        let silence = [0.0f32; FRAME];
        while self.frames * (FRAME as u64) < target {
            self.encode_frame(&silence)?;
        }
        if let Some((last, _)) = self.held.take() {
            self.writer
                .write_packet(last, self.serial, PacketWriteEndInfo::EndStream, target)
                .context("write final Ogg packet")?;
        }
        let secs = self.total_in as f32 / RATE as f32;
        Ok((self.writer.into_inner(), secs))
    }
}

// ---- decoding ----------------------------------------------------------------

/// Fully decoded voice note: interleaved 48 kHz i16.
pub struct Decoded {
    pub samples: Vec<i16>,
    pub channels: usize,
}

impl Decoded {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    pub fn duration_secs(&self) -> f32 {
        self.frames() as f32 / RATE as f32
    }
}

fn parse_opus_head(data: &[u8]) -> Result<(usize, usize)> {
    if data.len() < 19 || !data.starts_with(b"OpusHead") {
        bail!("not an Ogg/Opus stream (missing OpusHead)");
    }
    let channels = usize::from(data[9]);
    let pre_skip = usize::from(u16::from_le_bytes([data[10], data[11]]));
    if !(1..=2).contains(&channels) || data[18] != 0 {
        bail!("unsupported Opus channel layout ({channels} channels, mapping {})", data[18]);
    }
    Ok((channels, pre_skip))
}

pub fn decode_ogg_opus(data: &[u8]) -> Result<Decoded> {
    let mut reader = PacketReader::new(Cursor::new(data));
    let head = reader
        .read_packet()
        .context("read Ogg")?
        .ok_or_else(|| anyhow!("empty Ogg stream"))?;
    let (channels, pre_skip) = parse_opus_head(&head.data)?;
    let tags = reader
        .read_packet()
        .context("read Ogg")?
        .ok_or_else(|| anyhow!("Ogg stream has no OpusTags"))?;
    if !tags.data.starts_with(b"OpusTags") {
        bail!("missing OpusTags");
    }

    let ch = if channels == 1 { Channels::Mono } else { Channels::Stereo };
    let mut decoder = Decoder::new(RATE, ch).context("create Opus decoder")?;
    let mut buf = vec![0i16; MAX_DECODE_FRAMES * channels];
    let mut samples: Vec<i16> = Vec::new();
    let mut end_granule = None;
    let limit = MAX_PLAY_SECS * RATE as usize * channels;

    while let Some(pkt) = reader.read_packet().context("read Ogg")? {
        let n = decoder.decode(&pkt.data, &mut buf, false).context("Opus decode")?;
        samples.extend_from_slice(&buf[..n * channels]);
        if pkt.last_in_stream() {
            end_granule = Some(pkt.absgp_page());
        }
        if samples.len() > limit {
            bail!("voice note is too long to play ({} minutes max)", MAX_PLAY_SECS / 60);
        }
    }

    samples.drain(..(pre_skip * channels).min(samples.len()));
    if let Some(g) = end_granule.filter(|g| *g != u64::MAX) {
        let valid = usize::try_from(g).unwrap_or(usize::MAX).saturating_sub(pre_skip);
        samples.truncate(valid.saturating_mul(channels));
    }
    Ok(Decoded { samples, channels })
}

// ---- playback ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
    Playing,
    Paused,
    /// Finished, stopped or replaced by another voice note.
    Stopped,
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    State { id: String, state: PlayState, position: f32, duration: f32 },
    Failed { id: String, error: String },
}

enum PlayerCmd {
    Play { path: PathBuf, id: String },
    Pause,
    Stop,
}

/// Plays one voice note at a time on its own thread. Dropping the handle
/// shuts the thread down.
pub struct Player {
    tx: Sender<PlayerCmd>,
}

struct PlayCtl {
    playing: AtomicBool,
    finished: AtomicBool,
    pos_frames: AtomicUsize,
}

struct Active {
    id: String,
    ctl: Arc<PlayCtl>,
    duration: f32,
    _stream: Stream,
}

impl Player {
    pub fn new(on_event: impl Fn(PlayerEvent) + Send + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("audio-player".into())
            .spawn(move || player_loop(&rx, &on_event))
            .context("start the audio player thread")?;
        Ok(Self { tx })
    }

    /// Starts `path`, or resumes it when it is the paused current note.
    pub fn play(&self, path: PathBuf, id: String) {
        self.send(PlayerCmd::Play { path, id });
    }

    pub fn pause(&self) {
        self.send(PlayerCmd::Pause);
    }

    pub fn stop(&self) {
        self.send(PlayerCmd::Stop);
    }

    fn send(&self, cmd: PlayerCmd) {
        if self.tx.send(cmd).is_err() {
            log::warn!("audio player thread is gone");
        }
    }
}

fn player_loop(rx: &Receiver<PlayerCmd>, on_event: &dyn Fn(PlayerEvent)) {
    let mut cur: Option<Active> = None;
    loop {
        match rx.recv_timeout(TICK) {
            Ok(PlayerCmd::Play { path, id }) => {
                if let Some(a) = cur.as_ref().filter(|a| a.id == id) {
                    if !a.ctl.playing.swap(true, Ordering::AcqRel) {
                        emit_state(on_event, a, PlayState::Playing);
                    }
                    continue;
                }
                if let Some(old) = cur.take() {
                    emit_state(on_event, &old, PlayState::Stopped);
                }
                match start_playback(&path, &id) {
                    Ok(a) => {
                        emit_state(on_event, &a, PlayState::Playing);
                        cur = Some(a);
                    }
                    Err(e) => {
                        log::warn!("cannot play {}: {e:#}", path.display());
                        on_event(PlayerEvent::Failed { id, error: format!("{e:#}") });
                    }
                }
            }
            Ok(PlayerCmd::Pause) => {
                if let Some(a) = &cur {
                    if a.ctl.playing.swap(false, Ordering::AcqRel) {
                        emit_state(on_event, a, PlayState::Paused);
                    }
                }
            }
            Ok(PlayerCmd::Stop) => {
                if let Some(a) = cur.take() {
                    emit_state(on_event, &a, PlayState::Stopped);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(a) = &cur {
            if a.ctl.finished.load(Ordering::Acquire) {
                emit_state(on_event, a, PlayState::Stopped);
                cur = None;
            } else if a.ctl.playing.load(Ordering::Acquire) {
                emit_state(on_event, a, PlayState::Playing);
            }
        }
    }
}

fn emit_state(on_event: &dyn Fn(PlayerEvent), a: &Active, state: PlayState) {
    let position = if state == PlayState::Stopped {
        0.0
    } else {
        (a.ctl.pos_frames.load(Ordering::Relaxed) as f32 / RATE as f32).min(a.duration)
    };
    on_event(PlayerEvent::State { id: a.id.clone(), state, position, duration: a.duration });
}

fn start_playback(path: &Path, id: &str) -> Result<Active> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("read {}", path.display()))?
        .len();
    if len > MAX_PLAY_BYTES {
        bail!("the file is too large to play ({} MB max)", MAX_PLAY_BYTES / (1024 * 1024));
    }
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let decoded = Arc::new(decode_ogg_opus(&bytes)?);
    if decoded.frames() == 0 {
        bail!("the voice note is empty");
    }
    let duration = decoded.duration_secs();
    let ctl = Arc::new(PlayCtl {
        playing: AtomicBool::new(true),
        finished: AtomicBool::new(false),
        pos_frames: AtomicUsize::new(0),
    });
    let stream = open_output(decoded, ctl.clone())?;
    Ok(Active { id: id.to_string(), ctl, duration, _stream: stream })
}

fn open_output(data: Arc<Decoded>, ctl: Arc<PlayCtl>) -> Result<Stream> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| anyhow!("no audio output device"))?;
    let cfg = device.default_output_config().context("query the output device")?;
    let format = cfg.sample_format();
    let config: StreamConfig = cfg.into();
    let stream = match format {
        SampleFormat::F32 => build_output::<f32>(&device, config, data, ctl),
        SampleFormat::I16 => build_output::<i16>(&device, config, data, ctl),
        SampleFormat::I32 => build_output::<i32>(&device, config, data, ctl),
        SampleFormat::U16 => build_output::<u16>(&device, config, data, ctl),
        other => bail!("unsupported output sample format {other}"),
    }?;
    stream.play().context("start the output stream")?;
    Ok(stream)
}

fn build_output<T: SizedSample + FromSample<f32> + Send + 'static>(
    device: &Device,
    config: StreamConfig,
    data: Arc<Decoded>,
    ctl: Arc<PlayCtl>,
) -> Result<Stream> {
    let out_ch = usize::from(config.channels).max(1);
    let out_rate = config.sample_rate;
    let src_ch = data.channels;
    let frames = data.frames();
    let mut pos = 0f64;
    let silence = T::from_sample(0.0f32);

    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [T], _| {
                let playing = ctl.playing.load(Ordering::Acquire);
                let step = playback_step(RATE, out_rate, speed_at(SPEED_IDX.load(Ordering::Relaxed)));
                let sample = |f: usize, c: usize| {
                    f32::from(data.samples[f * src_ch + c.min(src_ch - 1)]) / 32768.0
                };
                for frame in out.chunks_mut(out_ch) {
                    let (l, r) = if playing && pos < frames as f64 {
                        let i = pos as usize;
                        let j = (i + 1).min(frames - 1);
                        let t = (pos - i as f64) as f32;
                        let l = sample(i, 0) * (1.0 - t) + sample(j, 0) * t;
                        let r = sample(i, 1) * (1.0 - t) + sample(j, 1) * t;
                        pos += step;
                        (l, r)
                    } else {
                        if pos >= frames as f64 {
                            ctl.finished.store(true, Ordering::Release);
                        }
                        (0.0, 0.0)
                    };
                    for (c, slot) in frame.iter_mut().enumerate() {
                        *slot = match (out_ch, c) {
                            (1, _) => T::from_sample((l + r) * 0.5),
                            (_, 0) => T::from_sample(l),
                            (_, 1) => T::from_sample(r),
                            _ => silence,
                        };
                    }
                }
                ctl.pos_frames.store(pos as usize, Ordering::Relaxed);
            },
            |e| {
                if is_transient_stream_error(&e) {
                    log::debug!("audio output glitch (ignored): {e}");
                } else {
                    log::error!("audio output error: {e}");
                }
            },
            None,
        )
        .context("open the output stream")?;
    Ok(stream)
}

// ---- recording ---------------------------------------------------------------

/// Streaming linear resampler (mono) used to bring capture audio to 48 kHz.
struct LinearResampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl LinearResampler {
    fn new(from_rate: u32) -> Self {
        Self { step: f64::from(from_rate) / f64::from(RATE), pos: 0.0, prev: 0.0 }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let Some(&last) = input.last() else { return };
        let n = input.len();
        let at = |i: isize| if i < 0 { self.prev } else { input[i as usize] };
        let mut produced = Vec::new();
        while self.pos < (n - 1) as f64 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let i = i as isize;
            produced.push(at(i) * (1.0 - t) + at(i + 1) * t);
            self.pos += self.step;
        }
        out.extend(produced);
        self.pos -= n as f64;
        self.prev = last;
    }
}

struct RecShared {
    /// f32 bits, 0..1.
    level: AtomicU32,
    /// Samples encoded so far at 48 kHz.
    samples: AtomicU64,
    stop: AtomicBool,
    cancel: AtomicBool,
    /// Ignored buffer underruns/overruns, for diagnostics.
    xruns: AtomicU32,
    /// Capture went silent and the thread ended the recording on its own.
    stalled: AtomicBool,
    error: Mutex<Option<String>>,
}

/// How long capture may deliver no samples before the recording is ended.
const STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// True when no audio chunk has arrived for [`STALL_TIMEOUT`] (mic switched or unplugged).
fn is_stalled(last_chunk: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_chunk) >= STALL_TIMEOUT
}

/// Stream errors that only mean a momentary glitch (buffer xrun, route change,
/// refused realtime priority): the stream keeps running, so they must not abort
/// a recording. Everything else (device lost, backend failure) is fatal.
fn is_transient_stream_error(e: &cpal::Error) -> bool {
    use cpal::ErrorKind;
    matches!(e.kind(), ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied)
}

/// Captures the default microphone into an Ogg/Opus file on its own thread.
pub struct Recorder {
    shared: Arc<RecShared>,
    path: PathBuf,
    handle: Option<JoinHandle<Result<f32>>>,
}

impl Recorder {
    /// Returns immediately; opening the device happens on the recorder
    /// thread and failures are reported through [`Recorder::take_error`].
    pub fn start(dir: &Path) -> Result<Self> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = dir.join(format!("voice-{millis}.ogg"));
        let shared = Arc::new(RecShared {
            level: AtomicU32::new(0),
            samples: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            xruns: AtomicU32::new(0),
            stalled: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        let thread_shared = shared.clone();
        let thread_path = path.clone();
        let dir = dir.to_path_buf();
        let handle = std::thread::Builder::new()
            .name("audio-recorder".into())
            .spawn(move || {
                let result = std::fs::create_dir_all(&dir)
                    .with_context(|| format!("create {}", dir.display()))
                    .and_then(|()| record_thread(&thread_path, &thread_shared));
                if let Err(e) = &result {
                    // Everything the recorder held is closed by now; drop the partial file.
                    match std::fs::remove_file(&thread_path) {
                        Ok(()) => {}
                        Err(r) if r.kind() == std::io::ErrorKind::NotFound => {}
                        Err(r) => log::warn!("could not delete the partial recording {}: {r}", thread_path.display()),
                    }
                    if let Ok(mut slot) = thread_shared.error.lock() {
                        *slot = Some(format!("{e:#}"));
                    }
                }
                result
            })
            .context("start the audio recorder thread")?;
        Ok(Self { shared, path, handle: Some(handle) })
    }

    /// Current input level, 0..1.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    pub fn elapsed_secs(&self) -> f32 {
        self.shared.samples.load(Ordering::Relaxed) as f32 / RATE as f32
    }

    /// The microphone stopped delivering audio and the recording ended by itself.
    pub fn ended_early(&self) -> bool {
        self.shared.stalled.load(Ordering::Acquire)
    }

    /// A setup or capture failure, once (e.g. no input device).
    pub fn take_error(&self) -> Option<String> {
        self.shared.error.lock().ok().and_then(|mut e| e.take())
    }

    /// Finishes the file. Blocks until the encoder has flushed, so call it
    /// off the UI thread.
    pub fn stop(mut self) -> Result<(PathBuf, f32)> {
        self.shared.stop.store(true, Ordering::Release);
        let handle = self.handle.take().ok_or_else(|| anyhow!("recorder already finished"))?;
        let secs = handle
            .join()
            .map_err(|_| anyhow!("the recorder thread panicked"))??;
        Ok((self.path.clone(), secs))
    }

    /// Stops capturing and deletes the partial file. Does not block.
    pub fn cancel(self) {
        drop(self);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Still holding the handle means nobody called `stop`: discard.
        if self.handle.is_some() {
            self.shared.cancel.store(true, Ordering::Release);
            self.shared.stop.store(true, Ordering::Release);
        }
    }
}

fn record_thread(path: &Path, shared: &Arc<RecShared>) -> Result<f32> {
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let (stream, input_rate) = open_input(tx, shared.clone())?;
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut enc = VoiceEncoder::new(BufWriter::new(file))?;
    let mut resampler = LinearResampler::new(input_rate);
    let mut resampled = Vec::new();

    let mut feed = |chunk: &[f32], enc: &mut VoiceEncoder<BufWriter<File>>| -> Result<()> {
        resampled.clear();
        resampler.process(chunk, &mut resampled);
        enc.push(&resampled)?;
        shared.samples.store(enc.samples(), Ordering::Relaxed);
        Ok(())
    };

    let mut last_chunk = Instant::now();
    while !shared.stop.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                last_chunk = Instant::now();
                feed(&chunk, &mut enc)?;
            }
            Err(RecvTimeoutError::Timeout) => {
                if is_stalled(last_chunk, Instant::now()) {
                    log::warn!("microphone delivered no audio for {STALL_TIMEOUT:?}; ending the recording early");
                    shared.stalled.store(true, Ordering::Release);
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(stream);
    while let Ok(chunk) = rx.try_recv() {
        feed(&chunk, &mut enc)?;
    }

    if shared.cancel.load(Ordering::Acquire) {
        drop(enc);
        if let Err(e) = std::fs::remove_file(path) {
            log::warn!("could not delete the cancelled recording {}: {e}", path.display());
        }
        return Ok(0.0);
    }
    let (mut w, secs) = enc.finish()?;
    w.flush().context("flush the recording")?;
    Ok(secs)
}

fn open_input(tx: Sender<Vec<f32>>, shared: Arc<RecShared>) -> Result<(Stream, u32)> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| anyhow!("no audio input device (microphone) found"))?;
    let cfg = device.default_input_config().context("query the input device")?;
    let format = cfg.sample_format();
    let config: StreamConfig = cfg.into();
    let rate = config.sample_rate;
    let stream = match format {
        SampleFormat::F32 => build_input::<f32>(&device, config, tx, shared),
        SampleFormat::I16 => build_input::<i16>(&device, config, tx, shared),
        SampleFormat::I32 => build_input::<i32>(&device, config, tx, shared),
        SampleFormat::U16 => build_input::<u16>(&device, config, tx, shared),
        other => bail!("unsupported input sample format {other}"),
    }?;
    stream.play().context("start the input stream")?;
    Ok((stream, rate))
}

fn build_input<T: SizedSample + Send + 'static>(
    device: &Device,
    config: StreamConfig,
    tx: Sender<Vec<f32>>,
    shared: Arc<RecShared>,
) -> Result<Stream>
where
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels).max(1);
    let err_shared = shared.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let mut mono = Vec::with_capacity(data.len() / channels);
                let mut sum_sq = 0.0f32;
                for frame in data.chunks(channels) {
                    let s = frame.iter().map(|x| x.to_sample::<f32>()).sum::<f32>() / frame.len() as f32;
                    sum_sq += s * s;
                    mono.push(s);
                }
                if !mono.is_empty() {
                    let rms = (sum_sq / mono.len() as f32).sqrt();
                    shared.level.store((rms * 6.0).min(1.0).to_bits(), Ordering::Relaxed);
                }
                // The receiver only disappears while the recording is being torn down.
                let _ = tx.send(mono);
            },
            move |e| {
                if is_transient_stream_error(&e) {
                    // Glitches are expected under load; the recording continues.
                    let n = err_shared.xruns.fetch_add(1, Ordering::Relaxed) + 1;
                    log::debug!("audio input glitch #{n} (ignored): {e}");
                    return;
                }
                log::error!("audio input error: {e}");
                // Let the recorder thread finalize what was captured.
                err_shared.stop.store(true, Ordering::Release);
                if let Ok(mut slot) = err_shared.error.lock() {
                    *slot = Some(format!("microphone error: {e}"));
                }
            },
            None,
        )
        .context("open the input stream")
}

/// Ogg/Opus bytes for a sine tone; used by demo data and tests.
#[cfg(any(test, debug_assertions))]
pub fn synth_tone_ogg(freq: f32, secs: f32) -> Result<Vec<u8>> {
    let total = (secs * RATE as f32) as usize;
    let tone: Vec<f32> = (0..total)
        .map(|i| 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin())
        .collect();
    let mut enc = VoiceEncoder::new(Vec::new())?;
    for chunk in tone.chunks(1000) {
        enc.push(chunk)?;
    }
    Ok(enc.finish()?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_cycles_through_all_three_and_wraps() {
        assert_eq!(next_speed_idx(0), 1);
        assert_eq!(next_speed_idx(1), 2);
        assert_eq!(next_speed_idx(2), 0);
        assert_eq!(speed_label_at(0), "1\u{00d7}");
        assert_eq!(speed_label_at(1), "1.5\u{00d7}");
        assert_eq!(speed_label_at(2), "2\u{00d7}");
        assert_eq!(speed_at(99), 2.0);
    }

    #[test]
    fn step_scales_with_speed_and_device_rate() {
        assert_eq!(playback_step(48_000, 48_000, 1.0), 1.0);
        assert_eq!(playback_step(48_000, 48_000, 2.0), 2.0);
        assert_eq!(playback_step(48_000, 44_100, 1.0), 48_000.0 / 44_100.0);
        // 1.5x: a 12 s note is consumed in 8 s of output frames.
        let step = playback_step(48_000, 48_000, 1.5);
        let out_frames = (12.0 * 48_000.0 / step).round();
        assert_eq!(out_frames, 8.0 * 48_000.0);
    }

    #[test]
    fn progress_stays_in_source_time_at_any_speed() {
        // Position is counted in source frames, so after 4 s of wall time at
        // 1.5x the note is at 6 s, i.e. half of a 12 s note.
        let step = playback_step(48_000, 48_000, 1.5);
        let pos_frames = step * 4.0 * 48_000.0;
        let pos_secs = pos_frames as f32 / RATE as f32;
        assert!((pos_secs - 6.0).abs() < 1e-3);
        assert!((pos_secs / 12.0 - 0.5).abs() < 1e-4);
    }

    /// Same algorithm as the engine's `ogg_opus_duration_secs`.
    fn engine_duration_secs(data: &[u8]) -> Option<u32> {
        let pos = data.windows(4).rposition(|w| w == b"OggS")?;
        let granule = u64::from_le_bytes(data.get(pos + 6..pos + 14)?.try_into().ok()?);
        Some((granule.saturating_add(24_000) / 48_000) as u32)
    }

    #[test]
    fn stall_predicate_triggers_after_timeout() {
        let t0 = Instant::now();
        assert!(!is_stalled(t0, t0));
        assert!(!is_stalled(t0, t0 + Duration::from_millis(1999)));
        assert!(is_stalled(t0, t0 + STALL_TIMEOUT));
        assert!(!is_stalled(t0 + Duration::from_secs(5), t0));
    }

    #[test]
    fn sine_roundtrips_through_ogg_opus() {
        let (ogg, secs) = {
            let total = RATE as usize;
            let tone: Vec<f32> = (0..total)
                .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin())
                .collect();
            let mut enc = VoiceEncoder::new(Vec::new()).expect("encoder");
            for chunk in tone.chunks(777) {
                enc.push(chunk).expect("push");
            }
            enc.finish().expect("finish")
        };
        assert!((secs - 1.0).abs() < 0.001);
        assert!(ogg.starts_with(b"OggS"));
        assert!(ogg.windows(8).any(|w| w == b"OpusHead"));
        assert!(ogg.windows(8).any(|w| w == b"OpusTags"));

        let decoded = decode_ogg_opus(&ogg).expect("decode");
        assert_eq!(decoded.channels, 1);
        assert!(
            (decoded.duration_secs() - 1.0).abs() <= 0.04,
            "duration {}",
            decoded.duration_secs()
        );
        let peak = decoded.samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(peak > 5_000, "decoded audio is silent (peak {peak})");
        assert_eq!(engine_duration_secs(&ogg), Some(1));
    }

    #[test]
    fn odd_lengths_and_empty_input_stay_valid() {
        let ogg = synth_tone_ogg(300.0, 0.5).expect("encode");
        let d = decode_ogg_opus(&ogg).expect("decode");
        assert!((d.duration_secs() - 0.5).abs() <= 0.04);

        let empty = VoiceEncoder::new(Vec::new()).expect("encoder").finish().expect("finish").0;
        let d = decode_ogg_opus(&empty).expect("decode empty");
        assert_eq!(d.frames(), 0);
    }

    #[test]
    fn resampler_scales_length() {
        let mut r = LinearResampler::new(44_100);
        let input = vec![0.25f32; 44_100];
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "len {}", out.len());
        assert!(out[10..].iter().all(|s| (s - 0.25).abs() < 1e-4));
    }

    #[test]
    fn xruns_are_not_fatal_but_device_loss_is() {
        use cpal::{Error, ErrorKind};
        assert!(is_transient_stream_error(&Error::new(ErrorKind::Xrun)));
        assert!(is_transient_stream_error(&Error::new(ErrorKind::DeviceChanged)));
        assert!(!is_transient_stream_error(&Error::new(ErrorKind::DeviceNotAvailable)));
        assert!(!is_transient_stream_error(&Error::new(ErrorKind::BackendError)));
        assert!(!is_transient_stream_error(&Error::with_message(ErrorKind::Other, "x")));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_ogg_opus(b"not an ogg file").is_err());
    }
}
