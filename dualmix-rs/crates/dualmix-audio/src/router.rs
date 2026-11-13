//! Virtual-devices engine, 1:1 port of C# `DualMix.Audio.AudioRouter`.
//!
//! Each virtual playback device ("DualMix Chat", and "DualMix Game" when a second cable exists) is captured with
//! event-driven WASAPI loopback (the 32-bit float mix of the Windows engine), resampled by a 160-tap windowed-sinc
//! resampler steered by a very slow critically damped PI loop on the real buffer level (clock drift), mapped by
//! speaker position, gain-ramped and written to the real output. Buffers grow by themselves after dropouts
//! (learned queue/source reserves) and the output is paused after ten minutes of digital silence.
//! Everything runs on one MMCSS "Pro Audio" thread; nothing is allocated, logged or locked while audio flows.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dualmix_core::channel_map::{self, ChannelMap};
use dualmix_core::log;
use dualmix_core::math::ChannelGains;
use dualmix_core::resampler::{SincResampler, TAPS};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST, WaitForMultipleObjects,
};
use windows::core::HSTRING;

use crate::com::{self, ComGuard};
use crate::format::{self, AudioFormat};
use crate::process_tuning::MmcssGuard;
use crate::LatencyProfile;

const IDLE_SUSPEND_MS: f64 = 600_000.0;
const SILENCE_THRESHOLD: f32 = 1e-6; // −120 dBFS
const STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
const STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
const BUFFERFLAGS_SILENT: u32 = 0x2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterState {
    Stopped = 0,
    Starting = 1,
    Running = 2,
    Error = 3,
}

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// Milliseconds since the process started (monotonic, like `Environment.TickCount64` differences).
pub fn tick_ms() -> i64 {
    epoch().elapsed().as_millis() as i64
}

#[inline]
fn now_s() -> f64 {
    epoch().elapsed().as_secs_f64()
}

struct AtomicF64(AtomicU64);

impl AtomicF64 {
    const fn zero() -> Self {
        Self(AtomicU64::new(0))
    }
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

struct AtomicF32(AtomicU32);

impl AtomicF32 {
    const fn new(v: u32) -> Self {
        Self(AtomicU32::new(v))
    }
    fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// State shared between the controller and the audio thread.
pub struct RouterShared {
    state: AtomicU8,
    game_gain: AtomicF32,
    chat_gain: AtomicF32,
    game_peak: AtomicF32,
    chat_peak: AtomicF32,
    output_peak: AtomicF32,
    latency_ms: AtomicF64,
    game_drift: AtomicF64,
    chat_drift: AtomicF64,
    max_late_ms: AtomicF64,
    learned_queue_ms: AtomicF64,
    learned_source_ms: AtomicF64,
    suspended: AtomicBool,
    real_time: AtomicBool,
    underruns: AtomicI64,
    output_starved: AtomicI64,
    game_empty: AtomicI64,
    chat_empty: AtomicI64,
    started_at: AtomicI64,
    last_error_code: AtomicI32,
    last_error: Mutex<Option<String>>,
    note: Mutex<Option<String>>,
    stage: Mutex<(String, Option<Instant>)>,
    formats: Mutex<(Option<AudioFormat>, Option<AudioFormat>, Option<AudioFormat>)>,
    ids: Mutex<(Option<String>, Option<String>, Option<String>)>,
}

impl RouterShared {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(RouterState::Stopped as u8),
            game_gain: AtomicF32::new(0x3F80_0000),
            chat_gain: AtomicF32::new(0x3F80_0000),
            game_peak: AtomicF32::new(0),
            chat_peak: AtomicF32::new(0),
            output_peak: AtomicF32::new(0),
            latency_ms: AtomicF64::zero(),
            game_drift: AtomicF64::zero(),
            chat_drift: AtomicF64::zero(),
            max_late_ms: AtomicF64::zero(),
            learned_queue_ms: AtomicF64::zero(),
            learned_source_ms: AtomicF64::zero(),
            suspended: AtomicBool::new(false),
            real_time: AtomicBool::new(false),
            underruns: AtomicI64::new(0),
            output_starved: AtomicI64::new(0),
            game_empty: AtomicI64::new(0),
            chat_empty: AtomicI64::new(0),
            started_at: AtomicI64::new(0),
            last_error_code: AtomicI32::new(0),
            last_error: Mutex::new(None),
            note: Mutex::new(None),
            stage: Mutex::new((String::new(), None)),
            formats: Mutex::new((None, None, None)),
            ids: Mutex::new((None, None, None)),
        }
    }

    fn set_state(&self, s: RouterState) {
        self.state.store(s as u8, Ordering::Release);
    }

    /// Marks the next start-up step; a step that took long is written to the log with its duration.
    fn step(&self, stage: &str) {
        let mut g = self.stage.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let (false, Some(since)) = (g.0.is_empty(), g.1) {
            let ms = now.duration_since(since).as_secs_f64() * 1000.0;
            if ms > 300.0 {
                log::warn(format!("Router: '{}' took {ms:.0} ms", g.0));
            }
        }
        *g = (stage.to_string(), Some(now));
    }

    fn set_note(&self, n: String) {
        if let Ok(mut g) = self.note.try_lock() {
            *g = Some(n);
        }
    }

    fn clear_peaks(&self) {
        self.game_peak.set(0.0);
        self.chat_peak.set(0.0);
        self.output_peak.set(0.0);
    }
}

/// Diagnostics hook: called on the audio thread with every mixed block (interleaved, frames, channels).
pub type Tap = Arc<dyn Fn(&[f32], usize, usize) + Send + Sync>;

pub struct AudioRouter {
    shared: Arc<RouterShared>,
    thread: Option<(JoinHandle<()>, Arc<AtomicBool>, Arc<AtomicBool>)>,
    stuck: Option<(JoinHandle<()>, Arc<AtomicBool>)>,
    pub tap: Option<Tap>,
}

impl Default for AudioRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRouter {
    pub fn new() -> Self {
        Self { shared: Arc::new(RouterShared::new()), thread: None, stuck: None, tap: None }
    }

    pub fn state(&self) -> RouterState {
        match self.shared.state.load(Ordering::Acquire) {
            1 => RouterState::Starting,
            2 => RouterState::Running,
            3 => RouterState::Error,
            _ => RouterState::Stopped,
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.shared.last_error.lock().ok().and_then(|g| g.clone())
    }
    pub fn last_error_code(&self) -> i32 {
        self.shared.last_error_code.load(Ordering::Relaxed)
    }
    pub fn game_peak(&self) -> f32 {
        self.shared.game_peak.get()
    }
    pub fn chat_peak(&self) -> f32 {
        self.shared.chat_peak.get()
    }
    pub fn output_peak(&self) -> f32 {
        self.shared.output_peak.get()
    }
    pub fn latency_ms(&self) -> f64 {
        self.shared.latency_ms.get()
    }
    pub fn game_drift_ppm(&self) -> f64 {
        self.shared.game_drift.get()
    }
    pub fn chat_drift_ppm(&self) -> f64 {
        self.shared.chat_drift.get()
    }
    pub fn suspended(&self) -> bool {
        self.shared.suspended.load(Ordering::Relaxed)
    }
    pub fn real_time_scheduling(&self) -> bool {
        self.shared.real_time.load(Ordering::Relaxed)
    }
    pub fn underruns(&self) -> i64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }
    pub fn output_starved(&self) -> i64 {
        self.shared.output_starved.load(Ordering::Relaxed)
    }
    pub fn game_empty(&self) -> i64 {
        self.shared.game_empty.load(Ordering::Relaxed)
    }
    pub fn chat_empty(&self) -> i64 {
        self.shared.chat_empty.load(Ordering::Relaxed)
    }
    pub fn max_late_ms(&self) -> f64 {
        self.shared.max_late_ms.get()
    }
    pub fn learned_queue_ms(&self) -> f64 {
        self.shared.learned_queue_ms.get()
    }
    pub fn learned_source_ms(&self) -> f64 {
        self.shared.learned_source_ms.get()
    }
    pub fn started_at(&self) -> i64 {
        self.shared.started_at.load(Ordering::Relaxed)
    }
    pub fn output_format(&self) -> Option<AudioFormat> {
        self.shared.formats.lock().ok().and_then(|g| g.0)
    }
    pub fn game_format(&self) -> Option<AudioFormat> {
        self.shared.formats.lock().ok().and_then(|g| g.1)
    }
    pub fn chat_format(&self) -> Option<AudioFormat> {
        self.shared.formats.lock().ok().and_then(|g| g.2)
    }
    pub fn game_id(&self) -> Option<String> {
        self.shared.ids.lock().ok().and_then(|g| g.0.clone())
    }
    pub fn chat_id(&self) -> Option<String> {
        self.shared.ids.lock().ok().and_then(|g| g.1.clone())
    }
    pub fn output_id(&self) -> Option<String> {
        self.shared.ids.lock().ok().and_then(|g| g.2.clone())
    }
    /// What the router thread is doing while it starts.
    pub fn stage(&self) -> String {
        self.shared.stage.lock().map(|g| g.0.clone()).unwrap_or_default()
    }

    pub fn dropout_detail(&self) -> String {
        let l = dualmix_core::loc::current();
        format!(
            "{} {}, {} {}, {} {}, {} {:.0} ms",
            l.pick("output starved", "uscita a secco"),
            self.output_starved(),
            l.pick("Game buffer empty", "buffer Game vuoto"),
            self.game_empty(),
            l.pick("Chat buffer empty", "buffer Chat vuoto"),
            self.chat_empty(),
            l.pick("longest audio thread delay", "ritardo massimo del thread audio"),
            self.max_late_ms()
        )
    }

    /// A message from the audio thread for the log (e.g. a reserve that was enlarged).
    pub fn take_note(&self) -> Option<String> {
        self.shared.note.lock().ok().and_then(|mut g| g.take())
    }

    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|(_, _, done)| !done.load(Ordering::Acquire))
    }

    /// A previous start never came back from Windows: no new start is attempted while that thread is stuck.
    pub fn blocked(&self) -> bool {
        self.stuck.as_ref().is_some_and(|(_, done)| !done.load(Ordering::Acquire))
    }

    pub fn set_gains(&self, g: ChannelGains) {
        self.shared.game_gain.set(g.game);
        self.shared.chat_gain.set(g.chat);
    }

    /// `game_id`: virtual Game device, or None when games play directly on the real output.
    pub fn start(&mut self, game_id: Option<&str>, chat_id: &str, output_id: &str, latency: LatencyProfile) -> Result<(), String> {
        self.stop();
        if self.blocked() {
            return Err(dualmix_core::loc::current()
                .pick(
                    "The audio engine is blocked: a previous request to Windows never returned.",
                    "Il motore audio è bloccato: una richiesta precedente a Windows non è mai tornata.",
                )
                .into());
        }
        if Some(output_id) == game_id || output_id == chat_id {
            return Err("The real output cannot be one of the virtual devices (it would create a loop).".into());
        }
        if game_id == Some(chat_id) {
            return Err("Game and Chat must be two different virtual devices.".into());
        }
        if let Some((h, done)) = self.stuck.take() {
            if done.load(Ordering::Acquire) {
                let _ = h.join();
            } else {
                self.stuck = Some((h, done));
            }
        }
        let sh = &self.shared;
        *sh.ids.lock().unwrap_or_else(|e| e.into_inner()) = (game_id.map(str::to_string), Some(chat_id.to_string()), Some(output_id.to_string()));
        sh.underruns.store(0, Ordering::Relaxed);
        sh.output_starved.store(0, Ordering::Relaxed);
        sh.game_empty.store(0, Ordering::Relaxed);
        sh.chat_empty.store(0, Ordering::Relaxed);
        sh.max_late_ms.set(0.0);
        sh.set_state(RouterState::Starting);
        sh.started_at.store(tick_ms(), Ordering::Relaxed);
        *sh.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
        sh.last_error_code.store(0, Ordering::Relaxed);

        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (shared, stop2, done2, tap) = (Arc::clone(&self.shared), Arc::clone(&stop), Arc::clone(&done), self.tap.clone());
        let (g, c, o) = (game_id.map(str::to_string), chat_id.to_string(), output_id.to_string());
        let h = std::thread::Builder::new()
            .name("DualMix.Router".into())
            .spawn(move || {
                run(&shared, &stop2, g.as_deref(), &c, &o, latency, tap);
                done2.store(true, Ordering::Release);
            })
            .map_err(|e| e.to_string())?;
        self.thread = Some((h, stop, done));
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some((h, stop, done)) = self.thread.take() {
            stop.store(true, Ordering::Release);
            let deadline = Instant::now() + Duration::from_millis(2000);
            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if done.load(Ordering::Acquire) {
                let _ = h.join();
            } else {
                log::warn(format!("Router thread did not stop in time (stage: '{}')", self.stage()));
                self.stuck = Some((h, done));
            }
        }
        if self.state() != RouterState::Error {
            self.shared.set_state(RouterState::Stopped);
        }
        self.shared.clear_peaks();
        self.shared.suspended.store(false, Ordering::Relaxed);
    }
}

impl Drop for AudioRouter {
    fn drop(&mut self) {
        self.stop();
    }
}

// ------------------------------------------------------------------------------------------ sample codec

/// Converts between a device sample format and float (TPDF dither on integer outputs).
struct SampleCodec {
    bits: u16,
    valid_bits: u16,
    float: bool,
    rng: u32,
}

impl SampleCodec {
    fn from(f: &AudioFormat) -> Result<Self, String> {
        if f.is_float && f.bits != 32 {
            return Err(format!("{}-bit float format not supported", f.bits));
        }
        if !f.is_float && !matches!(f.bits, 16 | 24 | 32) {
            return Err(format!("{}-bit PCM format not supported", f.bits));
        }
        Ok(Self { bits: f.bits, valid_bits: f.valid_bits, float: f.is_float, rng: 0x9E37_79B9 })
    }

    fn describe(&self) -> String {
        if self.float { "float32".into() } else { format!("pcm{}/{}", self.valid_bits, self.bits) }
    }

    unsafe fn read(&self, src: *const u8, dst: &mut [f32], samples: usize) {
        unsafe {
            if self.float {
                std::ptr::copy_nonoverlapping(src as *const f32, dst.as_mut_ptr(), samples);
                return;
            }
            match self.bits {
                16 => {
                    let p = src as *const i16;
                    for (i, d) in dst[..samples].iter_mut().enumerate() {
                        *d = p.add(i).read_unaligned() as f32 / 32768.0;
                    }
                }
                24 => {
                    for (i, d) in dst[..samples].iter_mut().enumerate() {
                        let b = src.add(i * 3);
                        let v = ((*b as i32) << 8) | ((*b.add(1) as i32) << 16) | ((*b.add(2) as i32) << 24);
                        *d = v as f32 / 2_147_483_648.0;
                    }
                }
                _ => {
                    let p = src as *const i32;
                    for (i, d) in dst[..samples].iter_mut().enumerate() {
                        *d = p.add(i).read_unaligned() as f32 / 2_147_483_648.0;
                    }
                }
            }
        }
    }

    fn rand(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        self.rng as f64 / 4_294_967_296.0
    }

    unsafe fn write(&mut self, src: &[f32], samples: usize, dst: *mut u8) {
        unsafe {
            if self.float {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut f32, samples);
                return;
            }
            let bits = self.valid_bits.clamp(16, 32) as i32;
            let full = 2f64.powi(bits - 1);
            let lsb = 1.0 / full;
            for (i, &s) in src[..samples].iter().enumerate() {
                let mut x = s as f64;
                if bits < 32 {
                    x += (self.rand() - self.rand()) * lsb; // TPDF ±1 LSB
                }
                let v = (x * full).clamp(-full, full - 1.0);
                let q = v.round() as i64;
                match self.bits {
                    16 => (dst as *mut i16).add(i).write_unaligned(q as i16),
                    24 => {
                        let iv = q as i32;
                        let b = dst.add(i * 3);
                        *b = iv as u8;
                        *b.add(1) = (iv >> 8) as u8;
                        *b.add(2) = (iv >> 16) as u8;
                    }
                    _ => (dst as *mut i32).add(i).write_unaligned((q << (32 - bits)) as i32),
                }
            }
        }
    }
}

// ------------------------------------------------------------------------------------------ source

struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self, String> {
        unsafe { CreateEventW(None, false, false, None).map(Self).map_err(|e| e.to_string()) }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// One virtual device: event-driven loopback capture + drift-steered resampler + channel map.
struct Source {
    label: &'static str,
    capture: IAudioClient,
    capture_client: IAudioCaptureClient,
    codec: SampleCodec,
    in_ch: usize,
    in_rate: usize,
    out_ch: usize,
    map: ChannelMap,
    resampler: SincResampler,
    nominal_step: f64,
    packet_frames: f64,
    base_target: f64,
    interleaved: Vec<f32>,
    planar: Vec<Vec<f32>>,
    silent: bool,
    fresh: bool,
    extra: f64,
    avg: f64,
    integral: f64,
    step: f64,
    hold_until: f64,
    zero_run: usize,
    dry_at: f64,
    pending_dropout: bool,
    event: Event,
    has_event: bool,
    name: String,
    format: AudioFormat,
    average_buffer_ms: f64,
    last_signal: f64,
}

fn device_name(en: &IMMDeviceEnumerator, id: &str) -> String {
    unsafe {
        en.GetDevice(&HSTRING::from(id))
            .ok()
            .and_then(|d| d.OpenPropertyStore(STGM_READ).ok())
            .and_then(|s| com::read_string(&s, &com::PKEY_FRIENDLY_NAME))
            .unwrap_or_else(|| id.to_string())
    }
}

impl Source {
    fn open(sh: &RouterShared, label: &'static str, en: &IMMDeviceEnumerator, id: &str, output: &AudioFormat, out_period_frames: usize, latency: &LatencyProfile) -> Result<Self, windows::core::Error> {
        unsafe {
            sh.step(&format!("{label}: apertura"));
            let device = en.GetDevice(&HSTRING::from(id))?;
            let name = device_name(en, id);
            let mut client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            sh.step(&format!("{label}: formato"));
            let wave: *mut WAVEFORMATEX = client.GetMixFormat()?;
            let fmt = format::from_ptr(wave as *const u8).ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(0x88890008u32 as i32), "format"))?;
            let codec = SampleCodec::from(&fmt).map_err(|m| windows::core::Error::new(windows::core::HRESULT(0x88890008u32 as i32), m.as_str()))?;
            let mut period: i64 = 0;
            client.GetDevicePeriod(Some(&mut period), None)?;
            sh.step(&format!("{label}: Initialize (loopback)"));
            let event = Event::new().map_err(|m| windows::core::Error::new(windows::core::HRESULT(-1), m.as_str()))?;
            let mut has_event = true;
            let first = client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, STREAMFLAGS_LOOPBACK | STREAMFLAGS_EVENTCALLBACK, 2_000_000, 0, wave, None)
                .and_then(|_| client.SetEventHandle(event.0));
            if let Err(e) = first {
                // Old Windows builds cannot signal events for loopback streams: poll instead (the loop also
                // wakes for the output event, every device period).
                log::warn(format!("Router: event-driven loopback unavailable on {label}, polling instead | {e}"));
                client = device.Activate(CLSCTX_ALL, None)?;
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, STREAMFLAGS_LOOPBACK, 2_000_000, 0, wave, None)?;
                has_event = false;
            }
            CoTaskMemFree(Some(wave as *const _));
            sh.step(&format!("{label}: servizi"));
            let capture_client: IAudioCaptureClient = client.GetService()?;

            let in_ch = fmt.channels;
            let in_rate = fmt.sample_rate as usize;
            let nominal_step = in_rate as f64 / output.sample_rate as f64;
            let packet_frames = (period as f64 * in_rate as f64 / 1e7).max(1.0);
            let margin = in_rate as f64 * latency.source_margin_ms as f64 / 1000.0;
            // Level wanted right before a block is read: the block, whole capture packets, plus the margin.
            let base_target = out_period_frames as f64 * nominal_step + packet_frames * latency.source_packets + margin;
            let extra = (latency.extra_source_ms * in_rate as f64 / 1000.0).min(packet_frames * 2.0);
            let average_buffer_ms = ((latency.source_packets - 0.5) * packet_frames + margin + extra) * 1000.0 / in_rate as f64;
            Ok(Self {
                label,
                capture: client,
                capture_client,
                codec,
                in_ch,
                in_rate,
                out_ch: output.channels,
                map: ChannelMap::create(fmt, *output),
                resampler: SincResampler::new(in_ch, nominal_step * 1.003),
                nominal_step,
                packet_frames,
                base_target,
                interleaved: vec![0.0; 16384],
                planar: (0..in_ch).map(|_| vec![0.0; 8192]).collect(),
                silent: true,
                fresh: false,
                extra,
                avg: 0.0,
                integral: 0.0,
                step: nominal_step,
                hold_until: 0.0,
                zero_run: 0,
                dry_at: 0.0,
                pending_dropout: false,
                event,
                has_event,
                name,
                format: fmt,
                average_buffer_ms,
                last_signal: now_s(),
            })
        }
    }

    fn drift_ppm(&self) -> f64 {
        (self.step / self.nominal_step - 1.0) * 1e6
    }

    fn packet_ms(&self) -> f64 {
        self.packet_frames * 1000.0 / self.in_rate as f64
    }

    /// Fill level a sound starts from: the level wanted before a block is read, minus the packet that starts it.
    fn park(&self) -> f64 {
        self.base_target + self.extra - self.packet_frames
    }

    /// Moves every captured packet into the resampler; digital silence is not buffered ("parked").
    fn pump(&mut self, now: f64) -> windows::core::Result<()> {
        unsafe {
            let mut packet = self.capture_client.GetNextPacketSize()?;
            while packet > 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture_client.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let frames_us = frames as usize;
                if frames_us > 0 {
                    let mut zero = flags & BUFFERFLAGS_SILENT != 0;
                    if !zero {
                        let n = frames_us * self.in_ch;
                        if self.interleaved.len() < n {
                            self.interleaved.resize(n * 2, 0.0);
                        }
                        self.codec.read(data, &mut self.interleaved, n);
                        let peak = channel_map::peak(&self.interleaved[..n]);
                        zero = peak == 0.0;
                        if peak >= SILENCE_THRESHOLD {
                            self.last_signal = now;
                        }
                    }
                    if zero {
                        // Parked: nothing to keep. Playing: a pause inside the sound, kept to preserve its timing.
                        if !self.silent {
                            self.resampler.write_silence(frames_us);
                            self.zero_run += frames_us;
                        }
                    } else {
                        if self.silent {
                            self.resampler.reset();
                            self.resampler.write_silence(self.park().round().max(0.0) as usize);
                            self.silent = false;
                            self.fresh = true;
                            self.hold_until = now + 2.0;
                        }
                        if self.dry_at != 0.0 {
                            // Sound kept coming right after the buffer ran out: a real dropout on this source.
                            if (now - self.dry_at) * 1000.0 < self.packet_ms() * 4.0 {
                                self.pending_dropout = true;
                            }
                            self.dry_at = 0.0;
                        }
                        self.resampler.write_interleaved(&self.interleaved[..frames_us * self.in_ch], frames_us);
                        self.zero_run = 0;
                    }
                }
                self.capture_client.ReleaseBuffer(frames)?;
                packet = self.capture_client.GetNextPacketSize()?;
            }
        }
        if self.dry_at != 0.0 && (now - self.dry_at) * 1000.0 > self.packet_ms() * 6.0 {
            self.dry_at = 0.0; // nothing came: the sound simply ended
        }
        Ok(())
    }

    fn go_silent(&mut self) {
        self.resampler.reset();
        self.zero_run = 0;
        self.silent = true;
    }

    fn discard_silence(&mut self) {
        if !self.silent {
            self.go_silent();
        }
    }

    fn ensure_planar(&mut self, frames: usize) {
        for p in &mut self.planar {
            if p.len() < frames {
                p.resize(frames * 2, 0.0);
            }
        }
    }

    /// Resamples `frames` output frames, maps channels, applies the gain ramp and adds into `mix`.
    #[allow(clippy::too_many_arguments)]
    fn mix_into(&mut self, sh: &RouterShared, mix: &mut [f32], frames: usize, from: f32, to: f32, now: f64, dt: f64, empty: &AtomicI64) -> f32 {
        if self.pending_dropout {
            self.pending_dropout = false;
            empty.fetch_add(1, Ordering::Relaxed);
            sh.underruns.fetch_add(1, Ordering::Relaxed);
            if self.extra < self.packet_frames * 2.0 {
                // Keep a larger reserve from now on.
                self.extra = (self.extra + self.packet_frames * 0.5).min(self.packet_frames * 2.0);
                let ms = self.extra * 1000.0 / self.in_rate as f64;
                sh.learned_source_ms.set(sh.learned_source_ms.get().max(ms));
                sh.set_note(format!("{} buffer reserve enlarged by {ms:.0} ms after a dropout", self.label));
            }
        }
        if self.silent {
            return 0.0;
        }
        let mut pending = self.resampler.pending();
        let target = self.base_target + self.extra;
        if self.fresh {
            self.fresh = false;
            self.avg = pending;
        }
        // Only digital silence left, for a while already: park.
        if self.zero_run >= self.resampler.buffered() && self.zero_run > self.in_rate / 5 {
            self.go_silent();
            return 0.0;
        }
        if pending < frames as f64 * self.step + 1.0 {
            // Not enough for a whole block: the sound ended or, rarely, the buffer ran dry. Play what is left.
            let mut tail = 0.0;
            if self.zero_run < self.resampler.buffered() {
                self.resampler.write_silence((frames as f64 * self.step) as usize + TAPS + 8);
                self.ensure_planar(frames);
                let n = self.resampler.read(&mut self.planar, frames, self.step);
                for p in &mut self.planar {
                    p[n..frames].fill(0.0);
                }
                tail = self.map.mix_into(&self.planar, mix, frames, self.out_ch, from, to);
                self.dry_at = now;
            }
            self.go_silent();
            return tail;
        }
        if pending > target + self.in_rate as f64 * 0.08 {
            // Large backlog (this thread was stalled): jump back to the target instead of lagging behind.
            self.resampler.skip((pending - target) as usize);
            pending = self.resampler.pending();
            self.avg = pending;
        }
        // Clock loop on the real, averaged fill level: natural frequency 0.014 rad/s, damping 0.7.
        self.avg += (pending - self.avg) * (dt / 2.0).min(1.0);
        let e = (self.avg - target) / self.in_rate as f64; // seconds, positive = too much buffered
        let mut adj = self.integral;
        if now >= self.hold_until {
            self.integral = (self.integral + 0.0002 * e * dt).clamp(-0.001, 0.001);
            adj = (0.02 * e + self.integral).clamp(-0.002, 0.002);
        }
        self.step = self.nominal_step * (1.0 + adj);

        // A short pause inside the sound: nothing to compute, just keep time.
        if self.zero_run >= self.resampler.buffered() {
            if self.resampler.advance(frames, self.step) < frames {
                self.go_silent();
            }
            return 0.0;
        }
        self.ensure_planar(frames);
        let got = self.resampler.read(&mut self.planar, frames, self.step);
        if got < frames {
            for p in &mut self.planar {
                p[got..frames].fill(0.0);
            }
            self.go_silent();
        }
        self.map.mix_into(&self.planar, mix, frames, self.out_ch, from, to)
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        unsafe {
            let _ = self.capture.Stop();
        }
    }
}

// ------------------------------------------------------------------------------------------ thread

fn run(sh: &RouterShared, stop: &AtomicBool, game_id: Option<&str>, chat_id: &str, output_id: &str, latency: LatencyProfile, tap: Option<Tap>) {
    let _com = ComGuard::mta();
    let mmcss = MmcssGuard::enter_pro_audio();
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    sh.real_time.store(mmcss.active, Ordering::Relaxed);
    if !mmcss.active {
        log::warn("Router: Windows did not grant real-time audio scheduling (MMCSS); running at high priority instead");
    }
    let res = run_inner(sh, stop, game_id, chat_id, output_id, latency, tap);
    if let Err((code, msg)) = res {
        if !stop.load(Ordering::Acquire) {
            sh.last_error_code.store(code, Ordering::Relaxed);
            *sh.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg.clone());
            sh.set_state(RouterState::Error);
        }
        log::error(format!("Router stopped by error: {msg}"));
    }
    sh.clear_peaks();
    sh.suspended.store(false, Ordering::Relaxed);
    drop(mmcss);
}

type RunResult = Result<(), (i32, String)>;

fn werr(e: windows::core::Error) -> (i32, String) {
    let code = e.code().0;
    (code, com::describe_hresult(code))
}

#[allow(clippy::too_many_arguments)]
fn run_inner(sh: &RouterShared, stop: &AtomicBool, game_id: Option<&str>, chat_id: &str, output_id: &str, latency: LatencyProfile, tap: Option<Tap>) -> RunResult {
    unsafe {
        sh.step("uscita: apertura");
        let en = com::enumerator().map_err(werr)?;
        let out_dev = en.GetDevice(&HSTRING::from(output_id)).map_err(werr)?;
        let out_name = device_name(&en, output_id);
        let out_client: IAudioClient = out_dev.Activate(CLSCTX_ALL, None).map_err(werr)?;
        let out_wave: *mut WAVEFORMATEX = out_client.GetMixFormat().map_err(werr)?;
        let out_fmt = format::from_ptr(out_wave as *const u8).ok_or((0, "unreadable output format".to_string()))?;
        let mut out_codec = SampleCodec::from(&out_fmt).map_err(|m| (0, m))?;
        let mut period100: i64 = 0;
        out_client.GetDevicePeriod(Some(&mut period100), None).map_err(werr)?;

        sh.step("uscita: Initialize");
        let init = out_client.Initialize(AUDCLNT_SHAREMODE_SHARED, STREAMFLAGS_EVENTCALLBACK, latency.buffer_ms as i64 * 10_000, 0, out_wave, None);
        CoTaskMemFree(Some(out_wave as *const _));
        init.map_err(werr)?;
        sh.step("uscita: servizi");
        let ev = Event::new().map_err(|m| (0, m))?;
        out_client.SetEventHandle(ev.0).map_err(werr)?;
        let render: IAudioRenderClient = out_client.GetService().map_err(werr)?;
        let buffer_frames = out_client.GetBufferSize().map_err(werr)? as usize;
        let out_ch = out_fmt.channels;
        let out_rate = out_fmt.sample_rate as usize;
        let period_frames = ((period100 as f64 * out_rate as f64 / 1e7).round() as usize).max(1);
        let period_ms = period_frames as f64 * 1000.0 / out_rate as f64;
        let base_padding = ((period_frames as f64 * latency.padding_periods).round() as usize)
            .clamp(buffer_frames.min(period_frames + out_rate / 1000), buffer_frames);
        let max_padding = buffer_frames.min(base_padding.max(period_frames * 6));
        let mut target_padding = max_padding.min(base_padding + (latency.extra_queue_ms * out_rate as f64 / 1000.0) as usize);

        let mut game = match game_id {
            Some(g) => Some(Source::open(sh, "Game", &en, g, &out_fmt, period_frames, &latency).map_err(werr)?),
            None => None,
        };
        let mut chat = Source::open(sh, "Chat", &en, chat_id, &out_fmt, period_frames, &latency).map_err(werr)?;
        if stop.load(Ordering::Acquire) {
            return Ok(()); // stopped (or given up on) while Windows was still opening the devices
        }
        sh.step("avvio dei flussi");
        *sh.formats.lock().unwrap_or_else(|e| e.into_inner()) = (Some(out_fmt), game.as_ref().map(|g| g.format), Some(chat.format));

        // Estimated latency added by DualMix: one cable engine period (loopback), the source buffer, the
        // resampling filter delay and the audio queued in the output buffer.
        let source_latency = chat.packet_ms() + chat.average_buffer_ms + SincResampler::FILTER_DELAY as f64 * 1000.0 / chat.format.sample_rate as f64;
        let latency_of = |queued: usize| (source_latency + (queued as f64 - period_frames as f64 / 2.0) * 1000.0 / out_rate as f64).round();
        sh.latency_ms.set(latency_of(target_padding));
        sh.learned_queue_ms.set((target_padding - base_padding) as f64 * 1000.0 / out_rate as f64);
        sh.learned_source_ms.set(latency.extra_source_ms);
        log::info(format!(
            "Router: '{}' {} ({}), buffer {}, period {}, queued {} frames; {}chat '{}' {}; added latency ≈ {} ms; real-time scheduling: {}",
            out_name,
            format::describe(&out_fmt),
            out_codec.describe(),
            buffer_frames,
            period_frames,
            target_padding,
            match &game {
                Some(g) => format!("game '{}' {}; ", g.name, format::describe(&g.format)),
                None => "game direct on the output; ".into(),
            },
            chat.name,
            format::describe(&chat.format),
            sh.latency_ms.get(),
            if sh.real_time.load(Ordering::Relaxed) { "yes" } else { "NO" }
        ));

        // Pre-roll silence, then start everything.
        render.GetBuffer(target_padding as u32).map_err(werr)?;
        render.ReleaseBuffer(target_padding as u32, BUFFERFLAGS_SILENT).map_err(werr)?;
        if let Some(g) = &game {
            g.capture.Start().map_err(werr)?;
        }
        chat.capture.Start().map_err(werr)?;
        out_client.Start().map_err(werr)?;
        sh.set_state(RouterState::Running);
        sh.step("");

        let mut handles: Vec<HANDLE> = vec![ev.0];
        if chat.has_event {
            handles.push(chat.event.0);
        }
        if let Some(g) = &game {
            if g.has_event {
                handles.push(g.event.0);
            }
        }
        let mut mix = vec![0f32; buffer_frames * out_ch];
        let (mut game_cur, mut chat_cur) = (sh.game_gain.get(), sh.chat_gain.get());
        let (mut game_hold, mut chat_hold, mut out_hold) = (0f32, 0f32, 0f32);
        let mut now = now_s();
        let (mut last_out_event, mut last_publish, mut last_signal, mut last_starve, mut late_window_start) = (now, now, now, 0.0f64, now);
        let mut late_max = 0f64;
        let mut suspended = false;
        let mut warmup = 20; // blocks during which a late start is not counted as a dropout

        while !stop.load(Ordering::Acquire) {
            let w = WaitForMultipleObjects(&handles, false, 100);
            if stop.load(Ordering::Acquire) {
                break;
            }
            let idx = w.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            now = now_s();

            if let Some(g) = &mut game {
                g.pump(now).map_err(werr)?;
            }
            chat.pump(now).map_err(werr)?;
            last_signal = last_signal.max(game.as_ref().map(|g| g.last_signal).unwrap_or(0.0)).max(chat.last_signal);

            if suspended {
                if last_signal == now {
                    // Something started playing: bring the output back. The packet that woke us is kept.
                    out_client.Reset().map_err(werr)?;
                    render.GetBuffer(target_padding as u32).map_err(werr)?;
                    render.ReleaseBuffer(target_padding as u32, BUFFERFLAGS_SILENT).map_err(werr)?;
                    out_client.Start().map_err(werr)?;
                    suspended = false;
                    sh.suspended.store(false, Ordering::Relaxed);
                    last_out_event = now;
                    warmup = 20;
                } else {
                    if let Some(g) = &mut game {
                        g.discard_silence();
                    }
                    chat.discard_silence();
                }
                continue;
            }

            if idx != 0 {
                // Woken by a capture event (or the timeout): nothing to render yet.
                if (now - last_out_event) * 1000.0 > 2000.0 {
                    return Err((0x8000_05B4u32 as i32, dualmix_core::loc::current().pick("the audio output does not respond", "l'uscita audio non risponde").into()));
                }
                continue;
            }

            let since_last = (now - last_out_event) * 1000.0;
            last_out_event = now;
            let late = since_last - period_ms;
            if warmup == 0 && late > late_max {
                late_max = late;
            }

            let padding = out_client.GetCurrentPadding().map_err(werr)? as usize;
            let mut frames = (buffer_frames - padding.min(buffer_frames)).min(target_padding.saturating_sub(padding));
            if frames == 0 {
                continue;
            }
            if warmup > 0 {
                warmup -= 1;
            } else if padding == 0 {
                // The device ran out of audio: this thread was kept waiting for too long.
                sh.underruns.fetch_add(1, Ordering::Relaxed);
                sh.output_starved.fetch_add(1, Ordering::Relaxed);
                if last_starve != 0.0 && (now - last_starve) * 1000.0 < 120_000.0 && target_padding < max_padding {
                    // Twice within two minutes: this PC needs a longer queue. Grow it by half a period, for good.
                    target_padding = max_padding.min(target_padding + period_frames / 2);
                    frames = (buffer_frames - padding).min(target_padding - padding);
                    sh.latency_ms.set(latency_of(target_padding));
                    sh.learned_queue_ms.set((target_padding - base_padding) as f64 * 1000.0 / out_rate as f64);
                    sh.set_note(format!(
                        "output queue enlarged to {:.0} ms after repeated late wake-ups (latency now ≈ {} ms)",
                        target_padding as f64 * 1000.0 / out_rate as f64,
                        sh.latency_ms.get()
                    ));
                }
                last_starve = now;
            }

            let n = frames * out_ch;
            mix[..n].fill(0.0);
            let dt = frames as f64 / out_rate as f64;
            let (game_target, chat_target) = (sh.game_gain.get(), sh.chat_gain.get());
            if let Some(g) = &mut game {
                game_hold = game_hold.max(g.mix_into(sh, &mut mix, frames, game_cur, game_target, now, dt, &sh.game_empty));
            }
            chat_hold = chat_hold.max(chat.mix_into(sh, &mut mix, frames, chat_cur, chat_target, now, dt, &sh.chat_empty));
            game_cur = game_target;
            chat_cur = chat_target;
            out_hold = out_hold.max(channel_map::peak(&mix[..n]));

            if let Some(t) = &tap {
                t(&mix[..n], frames, out_ch);
            }

            let ptr = render.GetBuffer(frames as u32).map_err(werr)?;
            out_codec.write(&mix, n, ptr);
            render.ReleaseBuffer(frames as u32, 0).map_err(werr)?;

            if (now - last_publish) * 1000.0 >= 33.0 {
                sh.game_peak.set(game_hold);
                sh.chat_peak.set(chat_hold);
                sh.output_peak.set(out_hold);
                game_hold = 0.0;
                chat_hold = 0.0;
                out_hold = 0.0;
                sh.game_drift.set(game.as_ref().map(|g| g.drift_ppm()).unwrap_or(0.0));
                sh.chat_drift.set(chat.drift_ppm());
                last_publish = now;
                if late_max > sh.max_late_ms.get() {
                    sh.max_late_ms.set(late_max);
                }
                if (now - late_window_start) * 1000.0 > 600_000.0 {
                    sh.max_late_ms.set(late_max);
                    late_max = 0.0;
                    late_window_start = now;
                }
            }

            if (now - last_signal) * 1000.0 > IDLE_SUSPEND_MS {
                // Nothing but digital silence for a long while: stop the output so Windows can idle/sleep.
                out_client.Stop().map_err(werr)?;
                suspended = true;
                sh.suspended.store(true, Ordering::Relaxed);
                sh.clear_peaks();
            }
        }

        let _ = out_client.Stop();
        if !matches!(sh.state.load(Ordering::Acquire), 3) {
            sh.set_state(RouterState::Stopped);
        }
        log::info(format!(
            "Router stopped (dropouts: {} [{} starved, {} game empty, {} chat empty], clock correction chat {:+.1} ppm{}",
            sh.underruns.load(Ordering::Relaxed),
            sh.output_starved.load(Ordering::Relaxed),
            sh.game_empty.load(Ordering::Relaxed),
            sh.chat_empty.load(Ordering::Relaxed),
            chat.drift_ppm(),
            match &game {
                Some(g) => format!(", game {:+.1} ppm)", g.drift_ppm()),
                None => ")".into(),
            }
        ));
        Ok(())
    }
}
