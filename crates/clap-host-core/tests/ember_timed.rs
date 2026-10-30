use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap_host_core::audio::{Engine, TimedMidi};
use clap_host_core::clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_TRANSPORT, CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
    CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO,
    CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_PLAYING, clap_event_header,
    clap_event_transport,
};
use clap_host_core::{events, host, loader::Loader};

fn transport(start: u32) -> clap_event_transport {
    let mut value: clap_event_transport = unsafe { std::mem::zeroed() };
    value.header = clap_event_header {
        size: std::mem::size_of::<clap_event_transport>() as u32,
        time: 0,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_: CLAP_EVENT_TRANSPORT,
        flags: 0,
    };
    value.flags = CLAP_TRANSPORT_HAS_TEMPO
        | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
        | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
        | CLAP_TRANSPORT_HAS_TIME_SIGNATURE
        | CLAP_TRANSPORT_IS_PLAYING;
    value.song_pos_seconds = (f64::from(start) / 48_000.0 * (1_u64 << 31) as f64).round() as i64;
    value.song_pos_beats = value.song_pos_seconds * 2;
    value.tempo = 120.0;
    value.tsig_num = 4;
    value.tsig_denom = 4;
    value
}

#[test]
#[ignore = "requires EMBER_CLAP_PATH to a locally built Ember DLL"]
fn ember_renders_timed_note_without_audio_device() {
    let path = std::env::var("EMBER_CLAP_PATH").expect("set EMBER_CLAP_PATH");
    let loader = unsafe { Loader::open(&path) }.expect("open Ember");
    let plugin = loader
        .create(host::make_host(), Some("com.lx-audiolabs.ember"))
        .expect("create Ember");
    assert!(unsafe { (*plugin).init.expect("init")(plugin) });
    assert!(unsafe { (*plugin).activate.expect("activate")(plugin, 48_000.0, 1, 4_096) });

    let stop = Arc::new(AtomicBool::new(false));
    let mut engine = Engine::new(
        plugin,
        2,
        None,
        events::queue(),
        events::queue(),
        events::queue(),
        Arc::clone(&stop),
        Arc::new(AtomicBool::new(false)),
    );
    let note = TimedMidi {
        time: 1,
        data: [0x90, 60, 100],
        is_live: false,
    };
    let mut audio = [0.0_f32; 256];
    let result = engine.process_timed(&mut audio, &transport(0), &[note]);
    let first_frame = [audio[0], audio[1]];
    let mut energy = audio.iter().map(|x| f64::from(x.abs())).sum::<f64>();
    for block in 1..32 {
        let result = engine.process_timed(&mut audio, &transport(block * 128), &[]);
        assert!(result.is_ok(), "timed block {block}: {result:?}");
        energy += audio.iter().map(|x| f64::from(x.abs())).sum::<f64>();
    }
    stop.store(true, Ordering::Release);
    engine.process(&mut audio);
    drop(engine);
    unsafe {
        (*plugin).deactivate.expect("deactivate")(plugin);
        (*plugin).destroy.expect("destroy")(plugin);
    }
    assert!(result.is_ok(), "first timed block: {result:?}");
    assert_eq!(first_frame, [0.0, 0.0], "note began before frame 1");
    assert!(energy > 0.01, "Ember remained silent: {energy}");
}
