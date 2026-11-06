//! cpal output stream → `clap_process`, with MIDI drained from the ring buffer.

#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap_sys::{
    audio_buffer::clap_audio_buffer, events::clap_event_transport, plugin::clap_plugin,
    process::clap_process,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;

use crate::events::{
    Dialect, EvList, MAX_NOTE_OUT, NoteOutList, PluginNoteOut, PluginOutEvent, Queue, RawMidi,
    UiEvent, capturing_output_events, capturing_timed_output_events,
};
use crate::host::AudioThreadGuard;
use crate::loader::{self, PluginPtr};

/// Frames per `process()` call at most — also the `max_frames_count` we activate with.
pub const MAX_FRAMES: usize = 4096;
/// Maximum number of timed events accepted in one preallocated block.
pub const MAX_TIMED_EVENTS: usize = 256;

/// Opt-in observations of output callbacks. Durations are microseconds.
#[cfg(feature = "realtime-probe")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeSnapshot {
    pub first_frames: u64,
    pub min_frames: u64,
    pub max_frames: u64,
    pub max_gap_us: u64,
    pub max_work_us: u64,
    pub nonzero_callbacks: u64,
}

#[cfg(feature = "realtime-probe")]
struct ProbeCounters {
    first_frames: AtomicU64,
    min_frames: AtomicU64,
    max_frames: AtomicU64,
    max_gap_us: AtomicU64,
    max_work_us: AtomicU64,
    nonzero_callbacks: AtomicU64,
}

#[cfg(feature = "realtime-probe")]
impl Default for ProbeCounters {
    fn default() -> Self {
        Self {
            first_frames: AtomicU64::new(0),
            min_frames: AtomicU64::new(u64::MAX),
            max_frames: AtomicU64::new(0),
            max_gap_us: AtomicU64::new(0),
            max_work_us: AtomicU64::new(0),
            nonzero_callbacks: AtomicU64::new(0),
        }
    }
}

#[cfg(feature = "realtime-probe")]
impl ProbeCounters {
    fn record(&self, frames: u64, gap_us: Option<u64>, work_us: u64, nonzero: bool) {
        let _ = self
            .first_frames
            .compare_exchange(0, frames, Ordering::Relaxed, Ordering::Relaxed);
        self.min_frames.fetch_min(frames, Ordering::Relaxed);
        self.max_frames.fetch_max(frames, Ordering::Relaxed);
        if let Some(gap) = gap_us {
            self.max_gap_us.fetch_max(gap, Ordering::Relaxed);
        }
        self.max_work_us.fetch_max(work_us, Ordering::Relaxed);
        if nonzero {
            self.nonzero_callbacks.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> ProbeSnapshot {
        let min = self.min_frames.load(Ordering::Relaxed);
        ProbeSnapshot {
            first_frames: self.first_frames.load(Ordering::Relaxed),
            min_frames: if min == u64::MAX { 0 } else { min },
            max_frames: self.max_frames.load(Ordering::Relaxed),
            max_gap_us: self.max_gap_us.load(Ordering::Relaxed),
            max_work_us: self.max_work_us.load(Ordering::Relaxed),
            nonzero_callbacks: self.nonzero_callbacks.load(Ordering::Relaxed),
        }
    }
}

#[cfg(feature = "realtime-probe")]
struct CallbackProbe {
    counters: Arc<ProbeCounters>,
    previous: Option<Instant>,
}

#[cfg(feature = "realtime-probe")]
impl CallbackProbe {
    fn new(counters: Arc<ProbeCounters>) -> Self {
        Self {
            counters,
            previous: None,
        }
    }

    fn begin(&mut self) -> (Instant, Option<u64>) {
        let now = Instant::now();
        let gap = self
            .previous
            .map(|previous| now.duration_since(previous).as_micros() as u64);
        self.previous = Some(now);
        (now, gap)
    }

    fn finish<T: PartialEq + Default>(
        &self,
        started: Instant,
        gap_us: Option<u64>,
        data: &[T],
        channels: usize,
    ) {
        let work_us = started.elapsed().as_micros() as u64;
        self.counters.record(
            (data.len() / channels) as u64,
            gap_us,
            work_us,
            data.iter().any(|sample| *sample != T::default()),
        );
    }
}

/// One MIDI event at a sample offset in the next `process_timed` block.
#[derive(Clone, Copy, Debug)]
pub struct TimedMidi {
    pub time: u32,
    pub data: RawMidi,
    pub is_live: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TimedProcessError {
    InvalidBuffer,
    TooManyEvents,
    EventOutsideBlock,
    UnsortedEvents,
}

/// Stream options the user can override. An ASIO-enabled Windows build defaults
/// to ASIO; `backend: None` explicitly selects CPAL's platform default.
#[derive(Clone, Debug)]
pub struct StreamSettings {
    /// CPAL backend name, for example `wasapi` or `asio`.
    pub backend: Option<String>,
    pub sample_rate: Option<u32>,
    pub buffer_size: Option<u32>,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            backend: if cfg!(all(windows, feature = "asio")) {
                Some("asio".into())
            } else {
                None
            },
            sample_rate: None,
            buffer_size: None,
        }
    }
}

/// Buffer sizes (frames per callback) offered in the UI.
pub const BUFFER_SIZES: [u32; 6] = [64, 128, 256, 512, 1024, 2048];

/// Owns everything the audio thread touches. Audio and event buffers are
/// preallocated; full callback realtime safety requires separate verification.
pub struct Engine {
    plugin: PluginPtr,
    device_channels: usize,
    /// One channel buffer per (port, channel), flattened; kept alive for the
    /// pointers in `in_ports` / `out_ports`.
    bufs: Vec<Vec<f32>>,
    /// Channel pointer array the `clap_audio_buffer`s point into. Never read
    /// directly — it just has to stay alive and unmoved.
    _ptrs: Vec<*mut f32>,
    in_ports: Vec<clap_audio_buffer>,
    out_ports: Vec<clap_audio_buffer>,
    /// Channels of output port 0 — what the device actually hears.
    main_out_channels: usize,
    /// Index into `bufs` where output port 0 starts.
    main_out_offset: usize,
    events: EvList,
    /// Param id → the plugin's `clap_param_info.cookie`, echoed back in param
    /// events so the plugin can skip its id lookup. Looked up per event.
    param_cookies: Vec<(u32, *mut core::ffi::c_void)>,
    midi_rx: Queue<RawMidi>,
    ui_rx: Queue<UiEvent>,
    /// Plugin → host param events captured from `process()` output events.
    out_tx: Queue<PluginOutEvent>,
    /// Plugin → host note/MIDI events captured by `process_timed`.
    notes_out: NoteOutList,
    dialect: Dialect,
    steady_time: i64,
    /// Interleaved f32 samples from the capture thread, or `None` for silence.
    capture_buf: Option<Arc<ArrayQueue<f32>>>,
    /// Set by `Session::drop`; the next callback stops the plugin on this
    /// (audio) thread and acknowledges via `stopped`.
    stop: Arc<AtomicBool>,
    /// Ack from the audio thread: `stop_processing` has run (or was a no-op).
    stopped: Arc<AtomicBool>,
    /// `start_processing` has run on the audio thread.
    started: bool,
    /// `start_processing` returned false; stay silent instead of retrying.
    start_failed: bool,
}

// Safety: the Engine is built on the main thread and then moved into the cpal
// callback, where it lives on exactly one thread. Raw pointers are into its own
// buffers (and the plugin, which outlives the stream).
unsafe impl Send for Engine {}

impl Engine {
    // Eight constructor args (queues + stop flags) — a params struct would be ceremony.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        plugin: *const clap_plugin,
        device_channels: usize,
        capture_buf: Option<Arc<ArrayQueue<f32>>>,
        midi_rx: Queue<RawMidi>,
        ui_rx: Queue<UiEvent>,
        out_tx: Queue<PluginOutEvent>,
        stop: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
    ) -> Self {
        let in_counts = loader::audio_port_channels(plugin, true);
        let mut out_counts = loader::audio_port_channels(plugin, false);
        // A plugin with no audio-ports extension still has to be heard somehow.
        if out_counts.iter().sum::<u32>() == 0 {
            out_counts = vec![2];
        }

        let main_out_offset = in_counts.iter().sum::<u32>() as usize;
        let main_out_channels = out_counts[0].max(1) as usize;

        let total_ch = main_out_offset + out_counts.iter().sum::<u32>() as usize;
        let mut bufs: Vec<Vec<f32>> = (0..total_ch).map(|_| vec![0.0; MAX_FRAMES]).collect();
        // Neither Vec ever resizes again, so these pointers stay valid for the
        // Engine's lifetime — including after it is moved into the callback.
        let mut ptrs: Vec<*mut f32> = bufs.iter_mut().map(Vec::as_mut_ptr).collect();

        let mut next = 0;
        let mut port_bufs = |counts: &[u32]| {
            counts
                .iter()
                .map(|&ch| {
                    let base = unsafe { ptrs.as_mut_ptr().add(next) };
                    next += ch as usize;
                    clap_audio_buffer {
                        data32: base,
                        data64: ptr::null_mut(),
                        channel_count: ch,
                        latency: 0,
                        constant_mask: 0,
                    }
                })
                .collect::<Vec<_>>()
        };
        let in_ports = port_bufs(&in_counts);
        let out_ports = port_bufs(&out_counts);

        Self {
            plugin: PluginPtr(plugin),
            device_channels,
            bufs,
            _ptrs: ptrs,
            in_ports,
            out_ports,
            main_out_channels,
            main_out_offset,
            events: EvList::with_capacity(MAX_TIMED_EVENTS),
            param_cookies: loader::params(plugin)
                .iter()
                .map(|p| (p.id, p.cookie))
                .collect(),
            midi_rx,
            ui_rx,
            out_tx,
            notes_out: NoteOutList::with_capacity(MAX_NOTE_OUT),
            dialect: loader::note_dialect(plugin),
            steady_time: 0,
            capture_buf,
            stop,
            stopped,
            started: false,
            start_failed: false,
        }
    }

    /// Channels per port, `(inputs, outputs)` — for the startup banner.
    #[must_use]
    pub fn port_layout(&self) -> (Vec<u32>, Vec<u32>) {
        let chans = |ports: &[clap_audio_buffer]| ports.iter().map(|p| p.channel_count).collect();
        (chans(&self.in_ports), chans(&self.out_ports))
    }

    #[must_use]
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Note/MIDI events the plugin emitted during the last `process_timed` call.
    #[must_use]
    pub fn notes_out(&self) -> &[PluginNoteOut] {
        self.notes_out.as_slice()
    }

    /// cpal callback body. `data` is interleaved f32 for `device_channels`.
    pub fn process(&mut self, data: &mut [f32]) {
        self.process_output(data, |sample| sample);
    }

    /// cpal callback body for an interleaved signed 32-bit device stream.
    pub fn process_i32(&mut self, data: &mut [i32]) {
        self.process_output(data, |sample| {
            if !sample.is_finite() {
                return 0;
            }
            let scaled = sample * 2_147_483_648.0;
            if scaled >= i32::MAX as f32 {
                i32::MAX
            } else if scaled <= i32::MIN as f32 {
                i32::MIN
            } else {
                scaled as i32
            }
        });
    }

    fn process_output<T: Copy + Default>(&mut self, data: &mut [T], convert: impl Fn(f32) -> T) {
        let _audio_thread = AudioThreadGuard::enter();
        let _guard = CurrentPluginGuard;
        crate::thread_pool::set_current_plugin(self.plugin.0);
        if self.device_channels == 0 {
            return;
        }
        // CLAP spec: start/stop_processing run on the audio thread. The
        // session commands a stop via `stop`; this callback executes it here
        // and acks via `stopped`, then keeps emitting silence until dropped.
        if self.stop.load(Ordering::Acquire) {
            self.halt_processing();
            data.fill(T::default());
            return;
        }
        if !self.started && !self.start_failed {
            let p = self.plugin.0;
            let ok = unsafe { (*p).start_processing }.is_none_or(|start| unsafe { start(p) });
            if ok {
                self.started = true;
            } else {
                eprintln!("plugin.start_processing returned false — output is silent");
                self.start_failed = true;
            }
        }
        if self.start_failed {
            data.fill(T::default());
            return;
        }

        // ponytail: every queued MIDI message lands at frame 0 of the next block —
        // sample-accurate timestamps need cpal's OutputCallbackInfo, add if it matters.
        self.events.clear();
        // Keep room for the 32 UI events below; leave excess MIDI queued for
        // later blocks so the preallocated event list never grows here.
        for _ in 0..(MAX_TIMED_EVENTS - 32) {
            let Some(msg) = self.midi_rx.pop() else { break };
            self.events.push_midi(msg, self.dialect, 0);
        }
        // Cap UI events per block: a burst (slider drag) spreads over several
        // blocks instead of landing in one huge event list. Spec-legal, and it
        // bounds the list size for plugins that mishandle bursts (Vital crash,
        // see planning/optimization.md). The rest stays queued for later blocks.
        for _ in 0..32 {
            let Some(ev) = self.ui_rx.pop() else { break };
            match ev {
                UiEvent::Param { id, value } => {
                    let cookie = self
                        .param_cookies
                        .iter()
                        .find(|(pid, _)| *pid == id)
                        .map_or(ptr::null_mut(), |(_, c)| *c);
                    self.events.push_param(id, value, cookie, 0);
                }
                UiEvent::Midi(msg) => self.events.push_midi(msg, self.dialect, 0),
            }
        }

        let total = data.len() / self.device_channels;
        let mut done = 0;
        while done < total {
            let frames = (total - done).min(MAX_FRAMES);
            self.process_block(frames, None, false);
            self.interleave_as(data, done, frames, &convert);
            // Events belong to the first block only.
            self.events.clear();
            self.steady_time += frames as i64;
            done += frames;
        }
    }

    /// Process one prepared block without an audio device. The caller supplies
    /// a CLAP transport snapshot and sorted events with offsets in `[0, frames)`.
    /// Same-offset order is preserved. Queued live UI/MIDI messages are separate.
    pub fn process_timed(
        &mut self,
        data: &mut [f32],
        transport: &clap_event_transport,
        midi: &[TimedMidi],
    ) -> Result<(), TimedProcessError> {
        if self.device_channels == 0 || !data.len().is_multiple_of(self.device_channels) {
            return Err(TimedProcessError::InvalidBuffer);
        }
        let frames = data.len() / self.device_channels;
        if frames == 0 || frames > MAX_FRAMES {
            return Err(TimedProcessError::InvalidBuffer);
        }
        if midi.len() > MAX_TIMED_EVENTS {
            return Err(TimedProcessError::TooManyEvents);
        }
        if midi.iter().any(|event| event.time as usize >= frames) {
            return Err(TimedProcessError::EventOutsideBlock);
        }
        if midi.windows(2).any(|pair| pair[0].time > pair[1].time) {
            return Err(TimedProcessError::UnsortedEvents);
        }

        self.notes_out.clear();

        let _audio_thread = AudioThreadGuard::enter();
        let _guard = CurrentPluginGuard;
        crate::thread_pool::set_current_plugin(self.plugin.0);

        if self.stop.load(Ordering::Acquire) {
            self.halt_processing();
            data.fill(0.0);
            return Ok(());
        }
        if !self.started && !self.start_failed {
            let p = self.plugin.0;
            let ok = unsafe { (*p).start_processing }.is_none_or(|start| unsafe { start(p) });
            if ok {
                self.started = true;
            } else {
                eprintln!("plugin.start_processing returned false — output is silent");
                self.start_failed = true;
            }
        }
        if self.start_failed {
            data.fill(0.0);
            return Ok(());
        }

        self.events.clear();
        for event in midi {
            let flags = if event.is_live {
                clap_sys::events::CLAP_EVENT_IS_LIVE
            } else {
                0
            };
            self.events
                .push_midi_with_flags(event.data, self.dialect, event.time, flags);
        }
        self.process_block(frames, Some(transport), true);
        self.interleave(data, 0, frames);
        self.events.clear();
        self.steady_time += frames as i64;
        Ok(())
    }

    /// `stop_processing` on the audio thread, once. Acks unconditionally so
    /// the drop side never waits forever (even if we never started).
    fn halt_processing(&mut self) {
        if self.started {
            let p = self.plugin.0;
            if let Some(stop) = unsafe { (*p).stop_processing } {
                unsafe { stop(p) };
            }
            self.started = false;
        }
        self.stopped.store(true, Ordering::Release);
    }

    fn process_block(
        &mut self,
        frames: usize,
        transport: Option<&clap_event_transport>,
        capture_notes: bool,
    ) {
        // The plugin asked (maybe from its GUI thread) for a params flush;
        // the spec wants it on the audio thread, so it happens here.
        if crate::host::take_params_flush_requested() {
            loader::flush_params(self.plugin.0, &self.out_tx);
        }
        // Deinterleave captured input into plugin input port buffers.
        // Each frame contributes one sample per input channel in the ring buffer.
        if self.main_out_offset > 0 {
            if let Some(buf) = &self.capture_buf {
                for i in 0..frames {
                    for ch in 0..self.main_out_offset {
                        self.bufs[ch][i] = buf.pop().unwrap_or(0.0);
                    }
                }
            } else {
                for b in &mut self.bufs[..self.main_out_offset] {
                    b[..frames].fill(0.0);
                }
            }
        }
        for p in &mut self.in_ports {
            p.constant_mask = 0;
        }

        let in_ev = self.events.as_input_events();
        if capture_notes {
            self.notes_out.set_params(&self.out_tx);
        }
        let out_ev = if capture_notes {
            capturing_timed_output_events(&mut self.notes_out)
        } else {
            capturing_output_events(&self.out_tx)
        };

        let proc = clap_process {
            steady_time: self.steady_time,
            frames_count: frames as u32,
            transport: transport.map_or(ptr::null(), ptr::from_ref),
            audio_inputs: if self.in_ports.is_empty() {
                ptr::null()
            } else {
                self.in_ports.as_ptr()
            },
            audio_outputs: self.out_ports.as_mut_ptr(),
            audio_inputs_count: self.in_ports.len() as u32,
            audio_outputs_count: self.out_ports.len() as u32,
            in_events: &raw const in_ev,
            out_events: &raw const out_ev,
        };

        if let Some(process_fn) = unsafe { (*self.plugin.0).process } {
            unsafe { process_fn(self.plugin.0, &raw const proc) };
        }
    }

    /// Non-interleaved plugin output → interleaved device buffer. Fewer plugin
    /// channels than device channels: the last one is repeated (mono → stereo).
    fn interleave(&self, data: &mut [f32], frame_offset: usize, frames: usize) {
        self.interleave_as(data, frame_offset, frames, &|sample| sample);
    }

    fn interleave_as<T>(
        &self,
        data: &mut [T],
        frame_offset: usize,
        frames: usize,
        convert: &impl Fn(f32) -> T,
    ) {
        let dev = self.device_channels;
        for ch in 0..dev {
            let src = &self.bufs[self.main_out_offset + ch.min(self.main_out_channels - 1)];
            for i in 0..frames {
                data[(frame_offset + i) * dev + ch] = convert(src[i]);
            }
        }
    }
}

/// User-facing label for a cpal device (`description().name()`, cpal 0.18).
fn device_label(device: &cpal::Device) -> String {
    device
        .description()
        .map_or_else(|_| "<unnamed>".into(), |d| d.name().to_string())
}

fn choose_host(backend: Option<&str>) -> Result<cpal::Host, String> {
    match backend {
        None => Ok(cpal::default_host()),
        Some(name) => {
            let id = name
                .parse()
                .map_err(|_| format!("unknown or unavailable audio backend {name:?}"))?;
            cpal::host_from_id(id).map_err(|e| format!("audio backend {name:?}: {e}"))
        }
    }
}

fn selected_input_name<'a>(
    backend: Option<&str>,
    input_name: Option<&'a str>,
    output_name: &'a str,
) -> Result<Option<&'a str>, String> {
    if backend.is_some_and(|name| name.eq_ignore_ascii_case("asio")) {
        if let Some(name) = input_name
            && name != output_name
        {
            return Err(format!(
                "ASIO input {name:?} must use the selected output driver {output_name:?}"
            ));
        }
        return Ok(Some(output_name));
    }
    Ok(input_name)
}

/// Backend names accepted by `StreamSettings::backend` on this build.
#[must_use]
pub fn available_backends() -> Vec<String> {
    cpal::available_hosts()
        .into_iter()
        .map(|id| id.name().to_ascii_lowercase())
        .collect()
}

fn select_output_format(
    configs: &[cpal::SupportedStreamConfigRange],
    channels: u16,
    rate: u32,
) -> Option<cpal::SampleFormat> {
    [cpal::SampleFormat::F32, cpal::SampleFormat::I32]
        .into_iter()
        .find(|&format| {
            configs.iter().any(|c| {
                c.channels() == channels
                    && c.sample_format() == format
                    && c.min_sample_rate() <= rate
                    && rate <= c.max_sample_rate()
            })
        })
}

fn listed_sample_rates(
    configs: &[cpal::SupportedStreamConfigRange],
    channels: u16,
    default_rate: u32,
) -> Vec<u32> {
    const COMMON: [u32; 6] = [44100, 48000, 88200, 96000, 176400, 192000];
    let supports = |rate| select_output_format(configs, channels, rate).is_some();
    let mut rates: Vec<u32> = COMMON.into_iter().filter(|&rate| supports(rate)).collect();
    if supports(default_rate) {
        rates.retain(|&rate| rate != default_rate);
        rates.insert(0, default_rate);
    }
    rates
}

/// Clears the thread-pool's current-plugin pointer when `process` returns —
/// every early-return path in `process` drops this guard first.
struct CurrentPluginGuard;
impl Drop for CurrentPluginGuard {
    fn drop(&mut self) {
        crate::thread_pool::set_current_plugin(ptr::null());
    }
}

/// Names of the available output devices, in `open()` order.
#[must_use]
pub fn output_devices() -> Vec<String> {
    output_devices_for(None).unwrap_or_default()
}

/// Output devices exposed by the requested backend.
pub fn output_devices_for(backend: Option<&str>) -> Result<Vec<String>, String> {
    let devices = choose_host(backend)?
        .output_devices()
        .map_err(|e| e.to_string())?;
    Ok(devices.map(|d| device_label(&d)).collect())
}

/// Names of the available input devices.
#[must_use]
pub fn input_devices() -> Vec<String> {
    input_devices_for(None).unwrap_or_default()
}

/// Input devices exposed by the requested backend.
pub fn input_devices_for(backend: Option<&str>) -> Result<Vec<String>, String> {
    let devices = choose_host(backend)?
        .input_devices()
        .map_err(|e| e.to_string())?;
    Ok(devices.map(|d| device_label(&d)).collect())
}

/// Sample rates `device_name` (or the default output device) supports, the
/// device's current default rate first when its format is supported.
#[must_use]
pub fn sample_rates(device_name: Option<&str>) -> Vec<u32> {
    sample_rates_for(None, device_name).unwrap_or_default()
}

/// Sample rates available on one output device in the requested backend.
pub fn sample_rates_for(
    backend: Option<&str>,
    device_name: Option<&str>,
) -> Result<Vec<u32>, String> {
    let host = choose_host(backend)?;
    let device = match device_name {
        Some(want) => host
            .output_devices()
            .ok()
            .and_then(|mut devs| devs.find(|d| device_label(d) == want)),
        None => host.default_output_device(),
    };
    let Some(device) = device else {
        return Ok(Vec::new());
    };
    let Ok(default_cfg) = device.default_output_config() else {
        return Ok(Vec::new());
    };
    let configs: Vec<_> = device
        .supported_output_configs()
        .map(Iterator::collect)
        .unwrap_or_default();
    Ok(listed_sample_rates(
        &configs,
        default_cfg.channels(),
        default_cfg.sample_rate(),
    ))
}

/// A running stream plus the plugin activation that belongs to it. Dropping it
/// stops processing and deactivates, so switching devices is drop + `open`.
pub struct Session {
    // Output stream first: stops before the plugin is deactivated (see Drop).
    _stream: cpal::Stream,
    // Input stream second: can stop any time after output.
    _in_stream: Option<cpal::Stream>,
    plugin: PluginPtr,
    /// Audio-thread stop command + ack for the start/stop_processing
    /// handshake (owned by the engine inside the stream callback).
    stop: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    callbacks: Arc<AtomicU64>,
    stream_errors: Arc<AtomicU64>,
    #[cfg(feature = "realtime-probe")]
    probe_counters: Arc<ProbeCounters>,
    pub sample_rate: f64,
    pub device_channels: usize,
    pub in_ports: Vec<u32>,
    pub out_ports: Vec<u32>,
    pub dialect: Dialect,
}

impl Session {
    /// Number of output callbacks delivered since this stream started.
    #[must_use]
    pub fn callback_count(&self) -> u64 {
        self.callbacks.load(Ordering::Relaxed)
    }

    /// Number of output stream errors reported by the backend.
    #[must_use]
    pub fn stream_error_count(&self) -> u64 {
        self.stream_errors.load(Ordering::Relaxed)
    }

    /// Snapshot of opt-in output callback measurements.
    #[cfg(feature = "realtime-probe")]
    #[must_use]
    pub fn probe_snapshot(&self) -> ProbeSnapshot {
        self.probe_counters.snapshot()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The spec wants stop_processing on the audio thread. The stream is
        // still playing, so the next callback sees `stop`, stops the plugin
        // there and acks — normally within one buffer period.
        self.stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_millis(250);
        while !self.stopped.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let p = self.plugin.0;
        if !self.stopped.load(Ordering::Acquire) {
            // The backend never ran the callback again; last-resort off-thread
            // stop so deactivate doesn't precede stop_processing.
            eprintln!("warn: audio thread did not ack stop — calling stop_processing off-thread");
            if let Some(stop) = unsafe { (*p).stop_processing } {
                unsafe { stop(p) };
            }
        }
        if let Some(deactivate) = unsafe { (*p).deactivate } {
            unsafe { deactivate(p) };
        }
    }
}

/// Activate the plugin at the device's rate and start streaming. `device_name`
/// of `None` picks the default output device; `input_name` of `None` picks the
/// default input device. `settings` overrides sample rate and buffer size;
/// `None` fields use the device/backend defaults.
pub fn open(
    plugin: *const clap_plugin,
    device_name: Option<&str>,
    input_name: Option<&str>,
    midi_rx: Queue<RawMidi>,
    ui_rx: Queue<UiEvent>,
    out_tx: Queue<PluginOutEvent>,
    settings: &StreamSettings,
) -> Result<Session, String> {
    let audio_host = choose_host(settings.backend.as_deref())?;
    let device = match device_name {
        Some(want) => audio_host
            .output_devices()
            .map_err(|e| format!("output devices: {e}"))?
            .find(|d| device_label(d) == want)
            .ok_or_else(|| format!("no output device named {want:?}"))?,
        None => audio_host
            .default_output_device()
            .ok_or("no default output device")?,
    };
    let config = device
        .default_output_config()
        .map_err(|e| format!("output config: {e}"))?;

    // cpal 0.17+: SampleRate is a u32 alias (no .0 newtype).
    let sample_rate = settings.sample_rate.unwrap_or_else(|| config.sample_rate());
    let configs: Vec<_> = device
        .supported_output_configs()
        .map_err(|e| format!("output configs: {e}"))?
        .collect();
    let format =
        select_output_format(&configs, config.channels(), sample_rate).ok_or_else(|| {
            format!(
                "{} does not support {sample_rate} Hz / {} channels in f32 or i32",
                device_label(&device),
                config.channels()
            )
        })?;
    let device_channels = config.channels() as usize;

    // Open a capture stream when the plugin declares audio input ports.
    let in_ch_count = loader::audio_port_channels(plugin, true)
        .iter()
        .sum::<u32>() as usize;
    let (capture_buf, in_stream) = if in_ch_count > 0 {
        let output_name = device_label(&device);
        let chosen_input =
            selected_input_name(settings.backend.as_deref(), input_name, &output_name)?;
        match open_input(&audio_host, sample_rate, in_ch_count, chosen_input) {
            Ok((buf, stream)) => (Some(buf), Some(stream)),
            Err(e) => {
                eprintln!("warn: audio input: {e} — plugin inputs will be silence");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    if let Some(activate) = unsafe { (*plugin).activate }
        && !unsafe { activate(plugin, f64::from(sample_rate), 1, MAX_FRAMES as u32) }
    {
        return Err("plugin.activate returned false".into());
    }
    // `start_processing` is deliberately not called here: the CLAP spec wants
    // it on the audio thread, so the engine calls it before its first
    // process block (and stop_processing at the end, see `Session::drop`).

    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let callbacks = Arc::new(AtomicU64::new(0));
    let stream_errors = Arc::new(AtomicU64::new(0));
    #[cfg(feature = "realtime-probe")]
    let probe_counters = Arc::new(ProbeCounters::default());
    let mk_engine = || {
        Engine::new(
            plugin,
            device_channels,
            capture_buf.clone(),
            Queue::clone(&midi_rx),
            Queue::clone(&ui_rx),
            Queue::clone(&out_tx),
            Arc::clone(&stop),
            Arc::clone(&stopped),
        )
    };
    let engine = mk_engine();
    let (in_ports, out_ports) = engine.port_layout();
    let dialect = engine.dialect();

    let build = |engine: Engine, buffer_size: cpal::BufferSize| {
        let mut engine = engine;
        let callback_count = Arc::clone(&callbacks);
        let error_count = Arc::clone(&stream_errors);
        let stream_config = cpal::StreamConfig {
            channels: config.channels(),
            sample_rate,
            buffer_size,
        };
        match format {
            cpal::SampleFormat::F32 => {
                #[cfg(feature = "realtime-probe")]
                let mut probe = CallbackProbe::new(Arc::clone(&probe_counters));
                device.build_output_stream::<f32, _, _>(
                    stream_config,
                    move |data: &mut [f32], _| {
                        #[cfg(feature = "realtime-probe")]
                        let (started, gap_us) = probe.begin();
                        engine.process(data);
                        #[cfg(feature = "realtime-probe")]
                        probe.finish(started, gap_us, data, device_channels);
                        callback_count.fetch_add(1, Ordering::Relaxed);
                    },
                    move |e| {
                        error_count.fetch_add(1, Ordering::Relaxed);
                        eprintln!("audio error: {e}");
                    },
                    None,
                )
            }
            cpal::SampleFormat::I32 => {
                #[cfg(feature = "realtime-probe")]
                let mut probe = CallbackProbe::new(Arc::clone(&probe_counters));
                device.build_output_stream::<i32, _, _>(
                    stream_config,
                    move |data: &mut [i32], _| {
                        #[cfg(feature = "realtime-probe")]
                        let (started, gap_us) = probe.begin();
                        engine.process_i32(data);
                        #[cfg(feature = "realtime-probe")]
                        probe.finish(started, gap_us, data, device_channels);
                        callback_count.fetch_add(1, Ordering::Relaxed);
                    },
                    move |e| {
                        error_count.fetch_add(1, Ordering::Relaxed);
                        eprintln!("audio error: {e}");
                    },
                    None,
                )
            }
            _ => unreachable!("format was selected from f32 and i32"),
        }
    };
    let requested = settings
        .buffer_size
        .map_or(cpal::BufferSize::Default, cpal::BufferSize::Fixed);
    let stream = match build(engine, requested) {
        Ok(stream) => stream,
        Err(first_err) => {
            let rollback = |e: String| {
                // The plugin was activated above but never started — roll back.
                if let Some(deactivate) = unsafe { (*plugin).deactivate } {
                    unsafe { deactivate(plugin) };
                }
                e
            };
            // Some backends (e.g. WASAPI shared) reject a fixed buffer size;
            // retry once with the backend default before giving up.
            if !matches!(requested, cpal::BufferSize::Fixed(_)) {
                return Err(rollback(format!("build_output_stream: {first_err}")));
            }
            eprintln!(
                "warn: fixed buffer size {} rejected ({first_err}) — using backend default",
                settings.buffer_size.unwrap_or_default()
            );
            match build(mk_engine(), cpal::BufferSize::Default) {
                Ok(stream) => stream,
                Err(e) => return Err(rollback(format!("build_output_stream: {e}"))),
            }
        }
    };
    if let Err(e) = stream.play() {
        if let Some(deactivate) = unsafe { (*plugin).deactivate } {
            unsafe { deactivate(plugin) };
        }
        return Err(format!("stream.play: {e}"));
    }

    Ok(Session {
        _stream: stream,
        _in_stream: in_stream,
        plugin: PluginPtr(plugin),
        stop,
        stopped,
        callbacks,
        stream_errors,
        #[cfg(feature = "realtime-probe")]
        probe_counters,
        sample_rate: f64::from(sample_rate),
        device_channels,
        in_ports,
        out_ports,
        dialect,
    })
}

/// Try to open an input device at `rate`, remixed to `plugin_in_ch` channels.
/// `want` of `None` picks the default input device.
/// Non-fatal: caller warns and falls back to silence on any error.
fn open_input(
    host: &cpal::Host,
    rate: cpal::SampleRate,
    plugin_in_ch: usize,
    want: Option<&str>,
) -> Result<(Arc<ArrayQueue<f32>>, cpal::Stream), String> {
    let dev = match want {
        Some(name) => host
            .input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| device_label(d) == name),
        None => host.default_input_device(),
    };
    let Some(device) = dev else {
        return Err(format!("input device '{want:?}' not found"));
    };
    let cfg = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!("input format {:?} is not f32", cfg.sample_format()));
    }
    let dev_ch = cfg.channels() as usize;
    let buf = Arc::new(ArrayQueue::<f32>::new(MAX_FRAMES * 16));
    let tx = Arc::clone(&buf);
    let stream_cfg = cpal::StreamConfig {
        channels: cfg.channels(),
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = device
        .build_input_stream::<f32, _, _>(
            stream_cfg,
            move |data: &[f32], _| {
                for frame in data.chunks(dev_ch) {
                    for ch in 0..plugin_in_ch {
                        // Repeat last device channel if plugin wants more than device has.
                        let s = frame.get(ch.min(dev_ch - 1)).copied().unwrap_or(0.0);
                        let _ = tx.push(s);
                    }
                }
            },
            |e| eprintln!("audio input error: {e}"),
            None,
        )
        .map_err(|e| format!("build_input_stream: {e}"))?;
    stream.play().map_err(|e| format!("stream.play: {e}"))?;
    eprintln!("audio in: {}", device_label(&device));
    Ok((buf, stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::events::{
        CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
        CLAP_EVENT_PARAM_VALUE,
        CLAP_EVENT_TRANSPORT, CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
        CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_PLAYING,
        clap_event_header, clap_event_midi, clap_event_note, clap_event_param_value,
        clap_event_transport, clap_output_events,
    };
    use crate::events::{PluginNoteOut, PluginOutEvent};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::thread::ThreadId;

    #[cfg(feature = "realtime-probe")]
    #[test]
    fn probe_records_actual_frames_timing_and_output_activity() {
        let counters = ProbeCounters::default();
        counters.record(512, None, 400, false);
        counters.record(480, Some(11_000), 650, true);
        assert_eq!(
            counters.snapshot(),
            ProbeSnapshot {
                first_frames: 512,
                min_frames: 480,
                max_frames: 512,
                max_gap_us: 11_000,
                max_work_us: 650,
                nonzero_callbacks: 1,
            }
        );
    }

    #[test]
    fn asio_default_input_stays_on_selected_output_driver() {
        assert_eq!(
            selected_input_name(Some("asio"), None, "Topping USB Audio Device"),
            Ok(Some("Topping USB Audio Device"))
        );
        assert_eq!(
            selected_input_name(
                Some("asio"),
                Some("Topping USB Audio Device"),
                "Topping USB Audio Device"
            ),
            Ok(Some("Topping USB Audio Device"))
        );
        assert!(
            selected_input_name(
                Some("asio"),
                Some("Voicemeeter"),
                "Topping USB Audio Device"
            )
            .is_err()
        );
        assert_eq!(
            selected_input_name(Some("wasapi"), None, "Topping USB Audio Device"),
            Ok(None)
        );
    }

    #[test]
    fn output_format_matches_requested_rate_and_channels() {
        let range = |channels, low, high, format| {
            cpal::SupportedStreamConfigRange::new(
                channels,
                low,
                high,
                cpal::SupportedBufferSize::Unknown,
                format,
            )
        };
        let configs = [
            range(2, 44_100, 44_100, cpal::SampleFormat::F32),
            range(1, 48_000, 48_000, cpal::SampleFormat::F32),
            range(2, 48_000, 48_000, cpal::SampleFormat::I32),
        ];
        assert_eq!(
            select_output_format(&configs, 2, 48_000),
            Some(cpal::SampleFormat::I32)
        );
        assert_eq!(select_output_format(&configs, 2, 96_000), None);
        assert_eq!(
            listed_sample_rates(&configs, 2, 96_000),
            vec![44_100, 48_000]
        );
    }

    #[cfg(not(feature = "asio"))]
    #[test]
    fn unavailable_asio_backend_is_an_error() {
        assert!(choose_host(Some("asio")).is_err());
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    fn asio_is_default_when_compiled_on_windows() {
        assert_eq!(StreamSettings::default().backend.as_deref(), Some("asio"));
    }

    /// (entry point, calling thread) pairs, in call order.
    static CALLS: Mutex<Vec<(&'static str, ThreadId)>> = Mutex::new(Vec::new());
    #[derive(Debug, PartialEq)]
    struct TimedObservation {
        steady_time: i64,
        header: (u32, u16, u16),
        flags: u32,
        beats: i64,
        seconds: i64,
        tempo: f64,
        signature: (u16, u16),
        events: Vec<(u32, u32)>,
    }
    static TIMED: Mutex<Vec<TimedObservation>> = Mutex::new(Vec::new());

    unsafe extern "C" fn fake_timed_process(_: *const clap_plugin, p: *const clap_process) -> i32 {
        let p = unsafe { &*p };
        let transport = unsafe { &*p.transport };
        let input = unsafe { &*p.in_events };
        let count = unsafe { input.size.unwrap()(input) };
        let events = (0..count)
            .map(|index| {
                let header: *const clap_event_header = unsafe { input.get.unwrap()(input, index) };
                let header = unsafe { &*header };
                (header.time, header.flags)
            })
            .collect();
        TIMED.lock().unwrap().push(TimedObservation {
            steady_time: p.steady_time,
            header: (
                transport.header.size,
                transport.header.space_id,
                transport.header.type_,
            ),
            flags: transport.flags,
            beats: transport.song_pos_beats,
            seconds: transport.song_pos_seconds,
            tempo: transport.tempo,
            signature: (transport.tsig_num, transport.tsig_denom),
            events,
        });
        1
    }

    #[test]
    fn timed_block_reaches_plugin_with_transport_and_note_offsets() {
        TIMED.lock().unwrap().clear();
        let mut plugin = fake_plugin();
        plugin.start_processing = None;
        plugin.process = Some(fake_timed_process);
        let mut engine = Engine::new(
            &raw const plugin,
            2,
            None,
            crate::events::queue(),
            crate::events::queue(),
            crate::events::queue(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        engine.dialect = Dialect::Midi;
        let mut transport: clap_event_transport = unsafe { std::mem::zeroed() };
        transport.header = clap_event_header {
            size: size_of::<clap_event_transport>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        };
        transport.flags = CLAP_TRANSPORT_HAS_TEMPO
            | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
            | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
            | CLAP_TRANSPORT_HAS_TIME_SIGNATURE
            | CLAP_TRANSPORT_IS_PLAYING;
        transport.song_pos_beats = 1_i64 << 31;
        transport.song_pos_seconds = 2_i64 << 31;
        transport.tempo = 120.0;
        transport.tsig_num = 4;
        transport.tsig_denom = 4;
        let notes = [
            TimedMidi {
                time: 1,
                data: [0x90, 60, 100],
                is_live: false,
            },
            TimedMidi {
                time: 64,
                data: [0x90, 64, 100],
                is_live: true,
            },
            TimedMidi {
                time: 127,
                data: [0x80, 60, 0],
                is_live: false,
            },
        ];
        let mut data = [0.0_f32; 256];
        engine.process_timed(&mut data, &transport, &notes).unwrap();
        transport.song_pos_beats += 1_i64 << 30;
        transport.song_pos_seconds += 1_i64 << 30;
        engine.process_timed(&mut data, &transport, &[]).unwrap();
        let calls = TIMED.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[
                TimedObservation {
                    steady_time: 0,
                    header: (
                        size_of::<clap_event_transport>() as u32,
                        CLAP_CORE_EVENT_SPACE_ID,
                        CLAP_EVENT_TRANSPORT
                    ),
                    flags: transport.flags,
                    beats: 1_i64 << 31,
                    seconds: 2_i64 << 31,
                    tempo: 120.0,
                    signature: (4, 4),
                    events: vec![(1, 0), (64, 1), (127, 0)],
                },
                TimedObservation {
                    steady_time: 128,
                    header: (
                        size_of::<clap_event_transport>() as u32,
                        CLAP_CORE_EVENT_SPACE_ID,
                        CLAP_EVENT_TRANSPORT
                    ),
                    flags: transport.flags,
                    beats: 3_i64 << 30,
                    seconds: 5_i64 << 30,
                    tempo: 120.0,
                    signature: (4, 4),
                    events: vec![],
                },
            ]
        );
    }

    fn playing_transport() -> clap_event_transport {
        let mut transport: clap_event_transport = unsafe { std::mem::zeroed() };
        transport.header = clap_event_header {
            size: size_of::<clap_event_transport>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        };
        transport.flags = CLAP_TRANSPORT_HAS_TEMPO
            | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
            | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
            | CLAP_TRANSPORT_HAS_TIME_SIGNATURE
            | CLAP_TRANSPORT_IS_PLAYING;
        transport.tempo = 120.0;
        transport.tsig_num = 4;
        transport.tsig_denom = 4;
        transport
    }

    fn engine_with(plugin: *const clap_plugin, out_tx: Queue<PluginOutEvent>) -> Engine {
        Engine::new(
            plugin,
            2,
            None,
            crate::events::queue(),
            crate::events::queue(),
            out_tx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    unsafe fn push_out(process: *const clap_process, header: *const clap_event_header) {
        let out = unsafe { &*(*process).out_events };
        unsafe { (out.try_push.unwrap())(out, header) };
    }

    unsafe extern "C" fn fake_push_note_and_midi(
        _: *const clap_plugin,
        process: *const clap_process,
    ) -> i32 {
        let note = clap_event_note {
            header: clap_event_header {
                size: size_of::<clap_event_note>() as u32,
                time: 5,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_ON,
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: 64,
            velocity: 1.0,
        };
        let midi = clap_event_midi {
            header: clap_event_header {
                size: size_of::<clap_event_midi>() as u32,
                time: 6,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI,
                flags: 0,
            },
            port_index: 0,
            data: [0x80, 64, 0],
        };
        unsafe {
            push_out(process, ptr::from_ref(&note.header));
            push_out(process, ptr::from_ref(&midi.header));
        }
        1
    }

    unsafe extern "C" fn fake_push_out_of_range_note(
        _: *const clap_plugin,
        process: *const clap_process,
    ) -> i32 {
        let frames = unsafe { (*process).frames_count };
        let note = clap_event_note {
            header: clap_event_header {
                size: size_of::<clap_event_note>() as u32,
                time: frames,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_ON,
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: 64,
            velocity: 1.0,
        };
        unsafe { push_out(process, ptr::from_ref(&note.header)) };
        1
    }

    unsafe extern "C" fn fake_push_param_and_note(
        _: *const clap_plugin,
        process: *const clap_process,
    ) -> i32 {
        let val = clap_event_param_value {
            header: clap_event_header {
                size: size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: 7,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.5,
        };
        let note = clap_event_note {
            header: clap_event_header {
                size: size_of::<clap_event_note>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_ON,
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: 64,
            velocity: 1.0,
        };
        unsafe {
            push_out(process, ptr::from_ref(&val.header));
            push_out(process, ptr::from_ref(&note.header));
        }
        1
    }

    static SCRIPT_LOCK: Mutex<()> = Mutex::new(());
    static SCRIPT: Mutex<Vec<Vec<PluginNoteOut>>> = Mutex::new(Vec::new());
    static SEEN_TRANSPORT: Mutex<Vec<(u32, i64, f64, u32)>> = Mutex::new(Vec::new());
    static PUSH_OK: AtomicUsize = AtomicUsize::new(0);
    static PUSH_FAIL: AtomicUsize = AtomicUsize::new(0);

    fn load_script(blocks: Vec<Vec<PluginNoteOut>>) {
        *SCRIPT.lock().unwrap() = blocks;
        SEEN_TRANSPORT.lock().unwrap().clear();
        PUSH_OK.store(0, AtomicOrdering::SeqCst);
        PUSH_FAIL.store(0, AtomicOrdering::SeqCst);
    }

    fn note_on(time: u32, key: i16) -> PluginNoteOut {
        PluginNoteOut::Note {
            type_: CLAP_EVENT_NOTE_ON,
            time,
            flags: 0,
            note_id: -1,
            port_index: 0,
            channel: 0,
            key,
            velocity: 1.0,
        }
    }

    fn note_off(time: u32, key: i16) -> PluginNoteOut {
        PluginNoteOut::Note {
            type_: CLAP_EVENT_NOTE_OFF,
            time,
            flags: 0,
            note_id: -1,
            port_index: 0,
            channel: 0,
            key,
            velocity: 0.0,
        }
    }

    fn note_type(ev: &PluginNoteOut) -> u16 {
        match ev {
            PluginNoteOut::Note { type_, .. } => *type_,
            PluginNoteOut::Midi { .. } => CLAP_EVENT_MIDI,
        }
    }

    fn note_time(ev: &PluginNoteOut) -> u32 {
        match ev {
            PluginNoteOut::Note { time, .. } | PluginNoteOut::Midi { time, .. } => *time,
        }
    }

    fn note_key(ev: &PluginNoteOut) -> i16 {
        match ev {
            PluginNoteOut::Note { key, .. } => *key,
            PluginNoteOut::Midi { .. } => -1,
        }
    }

    fn note_times(events: &[PluginNoteOut]) -> Vec<u32> {
        events.iter().map(note_time).collect()
    }

    fn note_keys(events: &[PluginNoteOut]) -> Vec<i16> {
        events.iter().map(note_key).collect()
    }

    fn seen_flags() -> Vec<u32> {
        SEEN_TRANSPORT
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.0)
            .collect()
    }

    fn seen_beats() -> Vec<i64> {
        SEEN_TRANSPORT
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.1)
            .collect()
    }

    fn seen_tempos() -> Vec<f64> {
        SEEN_TRANSPORT
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.2)
            .collect()
    }

    fn push_ok() -> usize {
        PUSH_OK.load(AtomicOrdering::SeqCst)
    }

    fn push_fail() -> usize {
        PUSH_FAIL.load(AtomicOrdering::SeqCst)
    }

    fn push_note_out(out: &clap_output_events, ev: PluginNoteOut) -> bool {
        match ev {
            PluginNoteOut::Note {
                type_,
                time,
                flags,
                note_id,
                port_index,
                channel,
                key,
                velocity,
            } => {
                let note = clap_event_note {
                    header: clap_event_header {
                        size: size_of::<clap_event_note>() as u32,
                        time,
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        type_,
                        flags,
                    },
                    note_id,
                    port_index,
                    channel,
                    key,
                    velocity,
                };
                unsafe { (out.try_push.unwrap())(out, ptr::from_ref(&note.header)) }
            }
            PluginNoteOut::Midi {
                time,
                flags,
                port_index,
                data,
            } => {
                let midi = clap_event_midi {
                    header: clap_event_header {
                        size: size_of::<clap_event_midi>() as u32,
                        time,
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        type_: CLAP_EVENT_MIDI,
                        flags,
                    },
                    port_index,
                    data,
                };
                unsafe { (out.try_push.unwrap())(out, ptr::from_ref(&midi.header)) }
            }
        }
    }

    unsafe extern "C" fn fake_scripted_process(
        _: *const clap_plugin,
        process: *const clap_process,
    ) -> i32 {
        let p = unsafe { &*process };
        let transport = unsafe { &*p.transport };
        SEEN_TRANSPORT.lock().unwrap().push((
            transport.flags,
            transport.song_pos_beats,
            transport.tempo,
            p.frames_count,
        ));
        let events = {
            let mut script = SCRIPT.lock().unwrap();
            if script.is_empty() {
                Vec::new()
            } else {
                script.remove(0)
            }
        };
        let out = unsafe { &*p.out_events };
        for ev in events {
            if push_note_out(out, ev) {
                PUSH_OK.fetch_add(1, AtomicOrdering::SeqCst);
            } else {
                PUSH_FAIL.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        1
    }

    fn scripted_plugin() -> clap_plugin {
        let mut plugin = fake_plugin();
        plugin.start_processing = None;
        plugin.process = Some(fake_scripted_process);
        plugin
    }

    #[test]
    fn timed_block_captures_plugin_note_and_midi_out() {
        let mut plugin = fake_plugin();
        plugin.start_processing = None;
        plugin.process = Some(fake_push_note_and_midi);
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        let transport = playing_transport();
        let mut data = [0.0_f32; 256];
        engine.process_timed(&mut data, &transport, &[]).unwrap();
        assert_eq!(
            engine.notes_out(),
            &[
                PluginNoteOut::Note {
                    type_: CLAP_EVENT_NOTE_ON,
                    time: 5,
                    flags: 0,
                    note_id: -1,
                    port_index: 0,
                    channel: 0,
                    key: 64,
                    velocity: 1.0
                },
                PluginNoteOut::Midi {
                    time: 6,
                    flags: 0,
                    port_index: 0,
                    data: [0x80, 64, 0]
                },
            ]
        );
    }

    #[test]
    fn live_process_does_not_capture_note_out() {
        let mut plugin = fake_plugin();
        plugin.start_processing = None;
        plugin.process = Some(fake_push_note_and_midi);
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        engine.process(&mut [0.0_f32; 256]);
        assert!(engine.notes_out().is_empty());
    }

    #[test]
    fn timed_capture_keeps_out_of_range_output_time() {
        let mut plugin = fake_plugin();
        plugin.start_processing = None;
        plugin.process = Some(fake_push_out_of_range_note);
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        let transport = playing_transport();
        let mut data = [0.0_f32; 256];
        engine.process_timed(&mut data, &transport, &[]).unwrap();
        match engine.notes_out() {
            [PluginNoteOut::Note {
                time: 128, key: 64, ..
            }] => {}
            other => panic!("expected time 128, got {other:?}"),
        }
    }

    #[test]
    fn timed_process_still_forwards_param_feedback() {
        let mut plugin = fake_plugin();
        plugin.start_processing = None;
        plugin.process = Some(fake_push_param_and_note);
        let out_tx = crate::events::queue();
        let mut engine = engine_with(&raw const plugin, Arc::clone(&out_tx));
        let transport = playing_transport();
        let mut data = [0.0_f32; 256];
        engine.process_timed(&mut data, &transport, &[]).unwrap();
        assert_eq!(
            out_tx.pop(),
            Some(PluginOutEvent::Value { id: 7, value: 0.5 })
        );
        assert_eq!(engine.notes_out().len(), 1);
    }

    #[test]
    fn timed_process_clears_notes_out_on_stop() {
        let mut plugin = fake_plugin();
        plugin.start_processing = None;
        plugin.process = Some(fake_push_note_and_midi);
        let stop = Arc::new(AtomicBool::new(false));
        let mut engine = Engine::new(
            &raw const plugin,
            2,
            None,
            crate::events::queue(),
            crate::events::queue(),
            crate::events::queue(),
            Arc::clone(&stop),
            Arc::new(AtomicBool::new(false)),
        );
        let transport = playing_transport();
        let mut data = [0.0_f32; 256];
        engine.process_timed(&mut data, &transport, &[]).unwrap();
        assert!(!engine.notes_out().is_empty());
        stop.store(true, Ordering::Release);
        engine.process_timed(&mut data, &transport, &[]).unwrap();
        assert!(engine.notes_out().is_empty());
    }

    #[test]
    fn timed_capture_event_at_last_legal_input_sample() {
        let _g = SCRIPT_LOCK.lock().unwrap();
        load_script(vec![vec![note_on(127, 60)]]);
        let plugin = scripted_plugin();
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        engine.dialect = Dialect::Midi;
        let mut data = [0.0_f32; 256];
        engine
            .process_timed(
                &mut data,
                &playing_transport(),
                &[TimedMidi {
                    time: 127,
                    data: [0x90, 60, 100],
                    is_live: false,
                }],
            )
            .unwrap();
        assert_eq!(note_times(engine.notes_out()), vec![127]);
    }

    #[test]
    fn timed_capture_preserves_same_sample_off_then_on() {
        let _g = SCRIPT_LOCK.lock().unwrap();
        load_script(vec![vec![note_off(10, 60), note_on(10, 60)]]);
        let plugin = scripted_plugin();
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        let mut data = [0.0_f32; 256];
        engine
            .process_timed(&mut data, &playing_transport(), &[])
            .unwrap();
        assert_eq!(
            engine
                .notes_out()
                .iter()
                .map(|e| (note_type(e), note_time(e), note_key(e)))
                .collect::<Vec<_>>(),
            vec![
                (CLAP_EVENT_NOTE_OFF, 10, 60),
                (CLAP_EVENT_NOTE_ON, 10, 60)
            ]
        );
    }

    #[test]
    fn timed_capture_stop_then_resume_emits_scripted_offs() {
        let _g = SCRIPT_LOCK.lock().unwrap();
        load_script(vec![
            vec![note_on(0, 60)],
            vec![note_off(0, 60)],
            vec![note_on(0, 60)],
        ]);
        let plugin = scripted_plugin();
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        let mut transport = playing_transport();
        let mut data = [0.0_f32; 256];
        engine
            .process_timed(&mut data, &transport, &[])
            .unwrap();
        assert_eq!(note_keys(engine.notes_out()), vec![60]);
        assert_eq!(note_type(&engine.notes_out()[0]), CLAP_EVENT_NOTE_ON);

        transport.flags &= !CLAP_TRANSPORT_IS_PLAYING;
        engine
            .process_timed(&mut data, &transport, &[])
            .unwrap();
        assert_eq!(note_type(&engine.notes_out()[0]), CLAP_EVENT_NOTE_OFF);

        transport.flags |= CLAP_TRANSPORT_IS_PLAYING;
        transport.song_pos_beats += 1_i64 << 30;
        engine
            .process_timed(&mut data, &transport, &[])
            .unwrap();
        assert_eq!(note_type(&engine.notes_out()[0]), CLAP_EVENT_NOTE_ON);
        assert_eq!(
            seen_flags(),
            vec![
                playing_transport().flags,
                playing_transport().flags & !CLAP_TRANSPORT_IS_PLAYING,
                playing_transport().flags,
            ]
        );
    }

    #[test]
    fn timed_capture_seek_and_loop_jump_are_visible_to_plugin() {
        let _g = SCRIPT_LOCK.lock().unwrap();
        load_script(vec![
            vec![note_on(0, 60)],
            vec![note_off(0, 60), note_on(0, 67)],
            vec![note_off(0, 67), note_on(0, 60)],
        ]);
        let plugin = scripted_plugin();
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        let mut transport = playing_transport();
        let mut data = [0.0_f32; 256];
        transport.song_pos_beats = 1_i64 << 31;
        engine
            .process_timed(&mut data, &transport, &[])
            .unwrap();
        transport.song_pos_beats = 4_i64 << 31;
        engine
            .process_timed(&mut data, &transport, &[])
            .unwrap();
        assert_eq!(note_keys(engine.notes_out()), vec![60, 67]);
        transport.song_pos_beats = 1_i64 << 31;
        engine
            .process_timed(&mut data, &transport, &[])
            .unwrap();
        assert_eq!(note_keys(engine.notes_out()), vec![67, 60]);
        assert_eq!(seen_beats(), vec![1_i64 << 31, 4_i64 << 31, 1_i64 << 31]);
    }

    #[test]
    fn timed_capture_tempo_change_across_blocks() {
        let _g = SCRIPT_LOCK.lock().unwrap();
        load_script(vec![vec![note_on(0, 60)], vec![note_on(0, 62)]]);
        let plugin = scripted_plugin();
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        let mut transport = playing_transport();
        let mut data = [0.0_f32; 128];
        transport.tempo = 120.0;
        engine
            .process_timed(&mut data, &transport, &[])
            .unwrap();
        transport.tempo = 140.0;
        engine
            .process_timed(&mut data, &transport, &[])
            .unwrap();
        match engine.notes_out() {
            [PluginNoteOut::Note { key: 62, .. }] => {}
            other => panic!("expected key 62, got {other:?}"),
        }
        assert_eq!(seen_tempos(), vec![120.0, 140.0]);
    }

    #[test]
    fn timed_capture_notes_out_overflow_stops_at_capacity() {
        let _g = SCRIPT_LOCK.lock().unwrap();
        let flood = vec![note_on(0, 60); MAX_NOTE_OUT + 1];
        load_script(vec![flood]);
        let plugin = scripted_plugin();
        let mut engine = engine_with(&raw const plugin, crate::events::queue());
        let mut data = [0.0_f32; 256];
        engine
            .process_timed(&mut data, &playing_transport(), &[])
            .unwrap();
        assert_eq!(engine.notes_out().len(), MAX_NOTE_OUT);
        assert_eq!(push_ok(), MAX_NOTE_OUT);
        assert_eq!(push_fail(), 1);
    }

    #[test]
    fn timed_block_rejects_invalid_offsets_and_buffers() {
        let plugin = fake_plugin();
        let mut engine = Engine::new(
            &raw const plugin,
            2,
            None,
            crate::events::queue(),
            crate::events::queue(),
            crate::events::queue(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let transport: clap_event_transport = unsafe { std::mem::zeroed() };
        let mut audio = [0.0_f32; 256];
        let note = |time| TimedMidi {
            time,
            data: [0x90, 60, 100],
            is_live: false,
        };
        assert_eq!(
            engine.process_timed(&mut audio[..3], &transport, &[]),
            Err(TimedProcessError::InvalidBuffer)
        );
        assert_eq!(
            engine.process_timed(&mut audio, &transport, &[note(128)]),
            Err(TimedProcessError::EventOutsideBlock)
        );
        assert_eq!(
            engine.process_timed(&mut audio, &transport, &[note(2), note(1)]),
            Err(TimedProcessError::UnsortedEvents)
        );
        assert_eq!(
            engine.process_timed(&mut audio, &transport, &vec![note(0); MAX_TIMED_EVENTS + 1]),
            Err(TimedProcessError::TooManyEvents)
        );
    }

    #[test]
    fn live_midi_burst_stays_queued_after_preallocated_block_is_full() {
        let mut plugin = fake_plugin();
        plugin.start_processing = None;
        plugin.process = None;
        let midi = crate::events::queue();
        for _ in 0..300 {
            midi.push([0x90, 60, 100]).unwrap();
        }
        let mut engine = Engine::new(
            &raw const plugin,
            2,
            None,
            Arc::clone(&midi),
            crate::events::queue(),
            crate::events::queue(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        engine.dialect = Dialect::Midi;
        engine.process(&mut [0.0_f32; 256]);
        assert_eq!(midi.len(), 300 - (MAX_TIMED_EVENTS - 32));
    }

    unsafe extern "C" fn fake_start(_: *const clap_plugin) -> bool {
        CALLS
            .lock()
            .unwrap()
            .push(("start", std::thread::current().id()));
        true
    }

    unsafe extern "C" fn fake_stop(_: *const clap_plugin) {
        CALLS
            .lock()
            .unwrap()
            .push(("stop", std::thread::current().id()));
    }

    unsafe extern "C" fn fake_process(_: *const clap_plugin, _: *const clap_process) -> i32 {
        CALLS
            .lock()
            .unwrap()
            .push(("process", std::thread::current().id()));
        0
    }

    /// Minimal vtable: no extensions (engine falls back to one stereo out
    /// port), but start/stop/process record their calling thread.
    fn fake_plugin() -> clap_plugin {
        clap_plugin {
            desc: ptr::null(),
            plugin_data: ptr::null_mut(),
            init: None,
            destroy: None,
            activate: None,
            deactivate: None,
            start_processing: Some(fake_start),
            stop_processing: Some(fake_stop),
            reset: None,
            process: Some(fake_process),
            get_extension: None,
            on_main_thread: None,
        }
    }

    /// The CLAP spec requires start/stop_processing on the audio thread —
    /// i.e. on the thread that runs `process` (here: this test thread under
    /// `AudioThreadGuard`).
    #[test]
    fn start_and_stop_processing_run_on_the_process_thread() {
        let plugin = fake_plugin();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let mut engine = Engine::new(
            &raw const plugin,
            2,
            None,
            crate::events::queue(),
            crate::events::queue(),
            crate::events::queue(),
            Arc::clone(&stop),
            Arc::clone(&stopped),
        );
        let here = std::thread::current().id();
        let mut data = vec![0.0_f32; 512];

        engine.process(&mut data);
        engine.process(&mut data);
        {
            let calls = CALLS.lock().unwrap();
            let on_this = |(n, t): &(&str, ThreadId)| *n == "process" && *t == here;
            assert!(calls.iter().any(on_this), "process ran: {calls:?}");
            assert_eq!(
                calls.iter().filter(|(n, _)| *n == "start").count(),
                1,
                "start_processing exactly once: {calls:?}"
            );
            assert!(
                calls
                    .iter()
                    .filter(|(n, _)| *n == "start")
                    .all(|(_, t)| *t == here),
                "start_processing on the process thread: {calls:?}"
            );
        }

        stop.store(true, Ordering::Release);
        engine.process(&mut data);
        let calls = CALLS.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|(n, _)| *n == "stop").count(),
            1,
            "stop_processing exactly once: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .filter(|(n, _)| *n == "stop")
                .all(|(_, t)| *t == here),
            "stop_processing on the process thread: {calls:?}"
        );
        assert!(stopped.load(Ordering::Acquire));
    }


}
