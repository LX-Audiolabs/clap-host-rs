//! cpal output stream → `clap_process`, with MIDI drained from the ring buffer.

#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap_sys::{audio_buffer::clap_audio_buffer, plugin::clap_plugin, process::clap_process};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;

use crate::events::{
    Dialect, EvList, PluginOutEvent, Queue, RawMidi, UiEvent, capturing_output_events,
};
use crate::host::mark_audio_thread;
use crate::loader::{self, PluginPtr};

/// Frames per `process()` call at most — also the `max_frames_count` we activate with.
pub const MAX_FRAMES: usize = 4096;

/// Stream options the user can override; `None` = device/backend default.
#[derive(Clone, Debug, Default)]
pub struct StreamSettings {
    pub sample_rate: Option<u32>,
    pub buffer_size: Option<u32>,
}

/// Buffer sizes (frames per callback) offered in the UI.
pub const BUFFER_SIZES: [u32; 6] = [64, 128, 256, 512, 1024, 2048];

/// Owns everything the audio thread touches. Buffers are allocated once; the
/// callback never allocates.
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
            events: EvList::with_capacity(256),
            param_cookies: loader::params(plugin)
                .iter()
                .map(|p| (p.id, p.cookie))
                .collect(),
            midi_rx,
            ui_rx,
            out_tx,
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

    /// cpal callback body. `data` is interleaved f32 for `device_channels`.
    pub fn process(&mut self, data: &mut [f32]) {
        mark_audio_thread();
        if self.device_channels == 0 {
            return;
        }
        // CLAP spec: start/stop_processing run on the audio thread. The
        // session commands a stop via `stop`; this callback executes it here
        // and acks via `stopped`, then keeps emitting silence until dropped.
        if self.stop.load(Ordering::Acquire) {
            self.halt_processing();
            data.fill(0.0);
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
            data.fill(0.0);
            return;
        }
        // ponytail: every queued MIDI message lands at frame 0 of the next block —
        // sample-accurate timestamps need cpal's OutputCallbackInfo, add if it matters.
        self.events.clear();
        while let Some(msg) = self.midi_rx.pop() {
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
            self.process_block(frames);
            self.interleave(data, done, frames);
            // Events belong to the first block only.
            self.events.clear();
            self.steady_time += frames as i64;
            done += frames;
        }
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

    fn process_block(&mut self, frames: usize) {
        // The plugin asked (maybe from its GUI thread) for a params flush;
        // the spec wants it on the audio thread, so it happens here.
        if crate::host::take_params_flush_requested() {
            loader::flush_params(self.plugin.0);
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
        let out_ev = capturing_output_events(&self.out_tx);

        let proc = clap_process {
            steady_time: self.steady_time,
            frames_count: frames as u32,
            transport: ptr::null(),
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
        let dev = self.device_channels;
        for ch in 0..dev {
            let src = &self.bufs[self.main_out_offset + ch.min(self.main_out_channels - 1)];
            for i in 0..frames {
                data[(frame_offset + i) * dev + ch] = src[i];
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

/// Names of the available output devices, in `open()` order.
#[must_use]
pub fn output_devices() -> Vec<String> {
    let Ok(devices) = cpal::default_host().output_devices() else {
        return Vec::new();
    };
    devices.map(|d| device_label(&d)).collect()
}

/// Names of the available input devices.
#[must_use]
pub fn input_devices() -> Vec<String> {
    let Ok(devices) = cpal::default_host().input_devices() else {
        return Vec::new();
    };
    devices.map(|d| device_label(&d)).collect()
}

/// Sample rates `device_name` (or the default output device) supports, the
/// device's current default rate first. Common rates only; falls back to just
/// the default when enumeration fails.
#[must_use]
pub fn sample_rates(device_name: Option<&str>) -> Vec<u32> {
    const COMMON: [u32; 6] = [44100, 48000, 88200, 96000, 176400, 192000];
    let host = cpal::default_host();
    let device = match device_name {
        Some(want) => host
            .output_devices()
            .ok()
            .and_then(|mut devs| devs.find(|d| device_label(d) == want)),
        None => host.default_output_device(),
    };
    let Some(device) = device else {
        return Vec::new();
    };
    let Ok(default_cfg) = device.default_output_config() else {
        return Vec::new();
    };
    let default_rate = default_cfg.sample_rate();
    let ranges: Vec<(u32, u32)> = device
        .supported_output_configs()
        .map(|it| {
            it.map(|c| (c.min_sample_rate(), c.max_sample_rate()))
                .collect()
        })
        .unwrap_or_default();
    let mut rates: Vec<u32> = COMMON
        .into_iter()
        .filter(|r| ranges.iter().any(|(lo, hi)| lo <= r && r <= hi))
        .collect();
    rates.retain(|&r| r != default_rate);
    rates.insert(0, default_rate);
    rates
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
    pub sample_rate: f64,
    pub device_channels: usize,
    pub in_ports: Vec<u32>,
    pub out_ports: Vec<u32>,
    pub dialect: Dialect,
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
    let audio_host = cpal::default_host();
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

    if config.sample_format() != cpal::SampleFormat::F32 {
        // ponytail: every current backend gives us f32; add conversion if one doesn't.
        return Err(format!(
            "unsupported sample format {:?} — add conversion if needed",
            config.sample_format()
        ));
    }

    // cpal 0.17+: SampleRate is a u32 alias (no .0 newtype).
    let sample_rate = settings.sample_rate.unwrap_or_else(|| config.sample_rate());
    if settings.sample_rate.is_some() {
        let supported = device
            .supported_output_configs()
            .map_err(|e| format!("output configs: {e}"))?
            .any(|c| c.min_sample_rate() <= sample_rate && sample_rate <= c.max_sample_rate());
        if !supported {
            return Err(format!(
                "sample rate {sample_rate} Hz not supported by {}",
                device_label(&device)
            ));
        }
    }
    let device_channels = config.channels() as usize;

    // Open a capture stream when the plugin declares audio input ports.
    let in_ch_count = loader::audio_port_channels(plugin, true)
        .iter()
        .sum::<u32>() as usize;
    let (capture_buf, in_stream) = if in_ch_count > 0 {
        match open_input(sample_rate, in_ch_count, input_name) {
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
        let stream_config = cpal::StreamConfig {
            channels: config.channels(),
            sample_rate,
            buffer_size,
        };
        device.build_output_stream::<f32, _, _>(
            stream_config,
            move |data: &mut [f32], _| engine.process(data),
            |e| eprintln!("audio error: {e}"),
            None,
        )
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
    rate: cpal::SampleRate,
    plugin_in_ch: usize,
    want: Option<&str>,
) -> Result<(Arc<ArrayQueue<f32>>, cpal::Stream), String> {
    let host = cpal::default_host();
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
    use std::sync::Mutex;
    use std::thread::ThreadId;

    /// (entry point, calling thread) pairs, in call order.
    static CALLS: Mutex<Vec<(&'static str, ThreadId)>> = Mutex::new(Vec::new());

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
    /// i.e. on the thread that runs `process` (here: this test thread, which
    /// `mark_audio_thread` registered as the audio thread).
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
