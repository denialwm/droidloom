//! Real PipeWire pool negotiation and tagged PCM round-trip on an isolated graph.
use droidloom_audio::{Options, serve_connection};
use droidloom_audio_client::Client;
use droidloom_audio_protocol::*;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct PipeWire {
    child: Child,
    policy: Option<Child>,
    directory: tempfile::TempDir,
    channels: u32,
    quantum: u32,
}
impl Drop for PipeWire {
    fn drop(&mut self) {
        if thread::panicking() && self.child.try_wait().ok().flatten().is_none() {
            eprintln!(
                "graph states: {:?}",
                self.ports()
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|o| matches!(
                        o["type"].as_str(),
                        Some("PipeWire:Interface:Node" | "PipeWire:Interface:Link")
                    ))
                    .map(|o| (
                        o["id"].clone(),
                        o["info"]["props"]["node.name"].clone(),
                        o["info"]["state"].clone(),
                        o["info"]["error"].clone()
                    ))
                    .collect::<Vec<_>>()
            );
            eprintln!(
                "PipeWire errors: {}",
                std::fs::read_to_string(self.directory.path().join("pipewire.log"))
                    .unwrap()
                    .lines()
                    .filter(|line| line.contains("[E]") || line.contains("[W]"))
                    .take(15)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        if let Some(policy) = &mut self.policy {
            let _ = policy.kill();
            let _ = policy.wait();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl PipeWire {
    fn new() -> Self {
        Self::with_channels(1)
    }
    fn with_channels(channels: u32) -> Self {
        Self::with_quantum(channels, 256)
    }
    fn with_quantum(channels: u32, quantum: u32) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("pipewire.conf");
        std::fs::write(
            &config,
            r#"
context.properties = {
  core.daemon = true
  core.name = pipewire-test
  default.clock.rate = 48000
  default.clock.quantum = 256
  default.clock.min-quantum = 256
  default.clock.max-quantum = 256
  log.level = 3
}
context.spa-libs = {
  audio.convert.* = audioconvert/libspa-audioconvert
  support.* = support/libspa-support
}
context.modules = [
  # Match normal audio-thread scheduling on this isolated graph. No desktop
  # session or RTKit connection is needed when the user has an RT limit.
  { name = libpipewire-module-rt args = {
    rt.prio = 88 rtportal.enabled = false rtkit.enabled = false
  } flags = [ ifexists nofail ] }
  { name = libpipewire-module-protocol-native }
  { name = libpipewire-module-spa-node-factory }
  { name = libpipewire-module-client-node }
  { name = libpipewire-module-adapter }
  { name = libpipewire-module-link-factory }
  { name = libpipewire-module-metadata }
  { name = libpipewire-module-access }
]
context.objects = [
  { factory = spa-node-factory args = {
    factory.name = support.node.driver node.name = Test-Driver
    node.group = pipewire.dummy priority.driver = 20000
  } }
  { factory = adapter args = {
    factory.name = support.null-audio-sink node.name = test.sink
    media.class = Audio/Sink audio.format = S16LE audio.rate = 48000
    audio.channels = 1 audio.position = [ MONO ]
    node.pause-on-idle = false
    adapter.auto-port-config = { mode = dsp monitor = true }
  } }
]
"#
            .replace("quantum = 256", &format!("quantum = {quantum}"))
            .replace(
                "audio.channels = 1 audio.position = [ MONO ]",
                if channels == 2 {
                    "audio.channels = 2 audio.position = [ FL FR ]"
                } else {
                    "audio.channels = 1 audio.position = [ MONO ]"
                },
            ),
        )
        .unwrap();
        let log = std::fs::File::create(directory.path().join("pipewire.log")).unwrap();
        let child = Command::new("pipewire")
            .args(["-c"])
            .arg(config)
            .env("XDG_RUNTIME_DIR", directory.path())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            policy: None,
            directory,
            channels,
            quantum,
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !server.remote().exists() {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "isolated PipeWire exited: {}",
                std::fs::read_to_string(server.directory.path().join("pipewire.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "PipeWire socket did not appear");
            thread::sleep(Duration::from_millis(10));
        }
        server
    }
    fn start_policy_without_sink(&mut self) {
        let objects = self.ports();
        let sink = objects
            .as_array()
            .unwrap()
            .iter()
            .find(|object| object["info"]["props"]["node.name"] == "test.sink")
            .unwrap()["id"]
            .as_u64()
            .unwrap();
        assert!(
            self.command("pw-cli")
                .args(["destroy", &sink.to_string()])
                .output()
                .unwrap()
                .status
                .success()
        );
        let log = std::fs::File::create(self.directory.path().join("wireplumber.log")).unwrap();
        self.policy = Some(
            self.command("wireplumber")
                .args(["--profile", "policy"])
                .env("XDG_CONFIG_HOME", self.directory.path())
                .env("XDG_STATE_HOME", self.directory.path())
                .env("XDG_CACHE_HOME", self.directory.path())
                .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
                .stdout(Stdio::null())
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                self.policy.as_mut().unwrap().try_wait().unwrap().is_none(),
                "policy exited: {}",
                std::fs::read_to_string(self.directory.path().join("wireplumber.log")).unwrap()
            );
            if self.ports().as_array().unwrap().iter().any(|object| {
                object["type"] == "PipeWire:Interface:Metadata"
                    && object["props"]["metadata.name"] == "sm-settings"
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "policy did not initialize");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn remote(&self) -> std::path::PathBuf {
        self.directory.path().join("pipewire-test")
    }
    fn command(&self, tool: &str) -> Command {
        let mut command = Command::new(tool);
        command
            .env("XDG_RUNTIME_DIR", self.directory.path())
            .env("PIPEWIRE_REMOTE", "pipewire-test");
        command
    }
    fn ports(&self) -> serde_json::Value {
        let output = self.command("pw-dump").output().unwrap();
        assert!(output.status.success(), "pw-dump failed");
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn link(&self, node: &str, playback: bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let dump = self.ports();
            let objects = dump.as_array().unwrap();
            let id = |name| {
                objects
                    .iter()
                    .find(|object| {
                        object["type"] == "PipeWire:Interface:Node"
                            && object["info"]["props"]["node.name"] == name
                    })
                    .map(|object| object["id"].as_u64().unwrap())
            };
            if let (Some(client), Some(sink)) = (id(node), id("test.sink")) {
                let port = |owner: u64, direction: &str| {
                    objects
                        .iter()
                        .filter(|object| {
                            object["type"] == "PipeWire:Interface:Port"
                                && object["info"]["props"]["node.id"].as_u64() == Some(owner)
                                && object["info"]["direction"] == direction
                        })
                        .collect::<Vec<_>>()
                };
                let endpoints = if playback {
                    (port(client, "output"), port(sink, "input"))
                } else {
                    (port(sink, "output"), port(client, "input"))
                };
                if endpoints.0.len() == self.channels as usize
                    && endpoints.1.len() == self.channels as usize
                {
                    for output in endpoints.0 {
                        let input = endpoints
                            .1
                            .iter()
                            .find(|input| {
                                input["info"]["props"]["audio.channel"]
                                    == output["info"]["props"]["audio.channel"]
                            })
                            .expect("missing matching audio channel");
                        let result = self
                            .command("pw-link")
                            .args([
                                output["id"].as_u64().unwrap().to_string(),
                                input["id"].as_u64().unwrap().to_string(),
                            ])
                            .output()
                            .unwrap();
                        assert!(
                            result.status.success(),
                            "pw-link failed: {}",
                            String::from_utf8_lossy(&result.stderr)
                        );
                    }
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "missing ports for {node}: {:?}",
                objects
                    .iter()
                    .filter(|o| o["type"] == "PipeWire:Interface:Port")
                    .map(|o| &o["info"]["props"])
                    .collect::<Vec<_>>()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn connect(
        &self,
        path: &Path,
        direction: Direction,
    ) -> (Client, thread::JoinHandle<io::Result<()>>) {
        self.connect_with_frames(path, direction, 1024)
    }
    fn connect_with_frames(
        &self,
        path: &Path,
        direction: Direction,
        frames: u32,
    ) -> (Client, thread::JoinHandle<io::Result<()>>) {
        let listener = SeqPacketListener::bind(path).unwrap();
        // SAFETY: reads the test process identity.
        let options = Options {
            peer_uid: unsafe { libc::geteuid() },
            allow_capture: true,
            target: Some("test.sink".into()),
            remote: Some(self.remote().to_str().unwrap().into()),
            capture_sink: true,
        };
        let worker = thread::spawn(move || serve_connection(listener.accept().unwrap(), &options));
        let client = Client::connect(
            path,
            Format {
                direction,
                rate: RATE,
                channels: self.channels,
                frames,
            },
        )
        .unwrap();
        (client, worker)
    }
}

#[test]
fn position_queries_before_linking_and_while_paused_keep_the_stream_alive() {
    let pipewire = PipeWire::new();
    let (mut playback, worker) = pipewire.connect(
        &pipewire.directory.path().join("startup-position.sock"),
        Direction::Playback,
    );
    let initial = playback.position().unwrap();
    assert_eq!(initial.frames, 0);
    assert!(initial.monotonic_ns > 0);
    playback.control(START).unwrap();
    assert_eq!(playback.position().unwrap().frames, 0);
    playback.control(PAUSE).unwrap();
    playback.control(FLUSH).unwrap();
    playback.control(STANDBY).unwrap();
    assert_eq!(playback.position().unwrap().frames, 0);
    pipewire.link(&playback.node_name(), true);
    playback.control(START).unwrap();
    for _ in 0..16 {
        let region = playback.acquire(1024).unwrap();
        assert_eq!(
            region.frames, 1024,
            "graph quantum must not truncate a HAL burst"
        );
        // SAFETY: the playback slot is exclusively held until submit.
        unsafe {
            std::slice::from_raw_parts_mut(region.pointer, region.frames as usize * 2).fill(0);
        }
        playback.submit(region.frames).unwrap();
        assert!(
            playback.position().unwrap().latency_ns <= 3 * 1024 * 1_000_000_000 / u64::from(RATE),
            "playback must not fill all four pool slots ahead of the graph"
        );
    }
    playback.control(DRAIN).unwrap();
    playback.control(PAUSE).unwrap();
    assert!(playback.position().unwrap().monotonic_ns >= initial.monotonic_ns);
    drop(playback);
    let _ = worker.join().unwrap();
}

#[test]
fn playback_without_a_link_keeps_a_clock() {
    let pipewire = PipeWire::new();
    let (mut playback, worker) = pipewire.connect_with_frames(
        &pipewire.directory.path().join("unlinked-playback.sock"),
        Direction::Playback,
        8192,
    );
    playback.control(START).unwrap();
    let start = Instant::now();
    let mut frames = 0;
    for _ in 0..96 {
        let region = playback.acquire(256).unwrap();
        // SAFETY: the acquired region is exclusively owned until submit.
        unsafe {
            std::slice::from_raw_parts_mut(region.pointer, region.frames as usize * 2).fill(0);
        }
        frames += region.frames;
        playback.submit(region.frames).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(300),
        "unlinked playback must be paced"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "unlinked playback must not time out"
    );
    assert!(playback.position().unwrap().frames > 0);
    assert_eq!(frames, 96 * 256);
    // A device arriving later must attach to the same healthy stream.
    let before_link = playback.position().unwrap().frames;
    pipewire.link(&playback.node_name(), true);
    for _ in 0..32 {
        let region = playback.acquire(256).unwrap();
        unsafe {
            std::slice::from_raw_parts_mut(region.pointer, region.frames as usize * 2).fill(0);
        }
        playback.submit(region.frames).unwrap();
    }
    assert!(playback.position().unwrap().frames >= before_link);
    playback.control(STANDBY).unwrap();
    drop(playback);
    let _ = worker.join().unwrap();
}

#[test]
fn stopped_playback_releases_the_dummy_clock() {
    let pipewire = PipeWire::new();
    let (mut playback, worker) = pipewire.connect(
        &pipewire.directory.path().join("idle-clock.sock"),
        Direction::Playback,
    );
    let driver_runs = |running: bool| {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let state = pipewire
                .ports()
                .as_array()
                .unwrap()
                .iter()
                .find(|object| object["info"]["props"]["node.name"] == "Test-Driver")
                .unwrap()["info"]["state"]
                .clone();
            if (state == "running") == running {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Test-Driver {state}, expected running: {running}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    };
    for stop in [STANDBY, PAUSE] {
        playback.control(START).unwrap();
        driver_runs(true);
        let region = playback.acquire(1024).unwrap();
        // SAFETY: the playback slot is exclusively held until submit.
        unsafe {
            std::slice::from_raw_parts_mut(region.pointer, region.frames as usize * 2).fill(0);
        }
        playback.submit(region.frames).unwrap();
        playback.control(stop).unwrap();
        driver_runs(false);
    }
    drop(playback);
    let _ = worker.join().unwrap();
}

#[test]
fn playback_without_devices_survives_session_policy() {
    let mut pipewire = PipeWire::new();
    pipewire.start_policy_without_sink();
    let (mut playback, worker) = pipewire.connect_with_frames(
        &pipewire.directory.path().join("no-device.sock"),
        Direction::Playback,
        8192,
    );
    playback.control(START).unwrap();
    let start = Instant::now();
    let pump = |playback: &mut Client| {
        for _ in 0..128 {
            let region = playback.acquire(256).unwrap();
            // SAFETY: this client owns the acquired sample region until submit.
            unsafe {
                std::slice::from_raw_parts_mut(region.pointer, region.frames as usize * 2).fill(0);
            }
            playback.submit(region.frames).unwrap();
            playback.position().unwrap();
        }
    };
    pump(&mut playback);
    assert!(start.elapsed() >= Duration::from_millis(500));
    assert!(playback.position().unwrap().frames > 0);
    // Let real session policy attach a newly available output to the existing
    // stream, then remove it again. Both transitions must preserve the clock.
    let held = playback.acquire(256).unwrap();
    let before_reconfigure = playback.position().unwrap().discontinuities;
    let result = pipewire.command("pw-cli").args(["create-node", "adapter",
        "{ factory.name = support.null-audio-sink node.name = test.sink media.class = Audio/Sink audio.format = S16LE audio.rate = 48000 audio.channels = 1 audio.position = [ MONO ] object.linger = true adapter.auto-port-config = { mode = dsp monitor = true } }"])
        .output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while playback.position().unwrap().discontinuities == before_reconfigure {
        assert!(
            Instant::now() < deadline,
            "policy did not reconfigure the stream"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // The pool mapping must remain writable even if its PipeWire wrapper was
    // removed while Android owned a credit. Submitting it releases the credit
    // without queuing stale audio into the new graph.
    unsafe {
        std::slice::from_raw_parts_mut(held.pointer, held.frames as usize * 2).fill(0);
    }
    playback.submit(held.frames).unwrap();
    pump(&mut playback);
    let objects = pipewire.ports();
    assert!(
        objects
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["type"] == "PipeWire:Interface:Link"
                && object["info"]["state"] == "active")
    );
    let sink = objects
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["info"]["props"]["node.name"] == "test.sink")
        .unwrap()["id"]
        .as_u64()
        .unwrap();
    let before_remove = playback.position().unwrap().frames;
    assert!(
        pipewire
            .command("pw-cli")
            .args(["destroy", &sink.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    );
    pump(&mut playback);
    assert!(playback.position().unwrap().frames > before_remove);
    playback.control(STANDBY).unwrap();
    let name = playback.node_name();
    drop(playback);
    let _ = worker.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while pipewire
        .ports()
        .as_array()
        .unwrap()
        .iter()
        .any(|object| object["info"]["props"]["node.name"] == name)
    {
        assert!(Instant::now() < deadline, "closed playback node leaked");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn dedicated_pool_carries_tagged_pcm_in_both_directions() {
    for channels in [1, 2] {
        tagged_pcm_round_trip(PipeWire::with_channels(channels), 1024);
    }
}

#[test]
fn short_playback_bursts_preserve_pcm_across_smaller_and_larger_graph_periods() {
    for quantum in [64, 256, 512, 1024] {
        tagged_pcm_round_trip(PipeWire::with_quantum(2, quantum), 256);
    }
}

fn tagged_pcm_round_trip(pipewire: PipeWire, burst: u32) {
    let channels = pipewire.channels as usize;
    let (mut playback, play_worker) = pipewire.connect_with_frames(
        &pipewire.directory.path().join("play.sock"),
        Direction::Playback,
        if burst == 256 { 8192 } else { burst },
    );
    let (mut capture, capture_worker) = pipewire.connect(
        &pipewire.directory.path().join("capture.sock"),
        Direction::Capture,
    );
    pipewire.link(&playback.node_name(), true);
    pipewire.link(&capture.node_name(), false);
    playback.control(START).unwrap();
    let mut primed = 0;
    if burst < pipewire.quantum {
        // Establish graph timing, then leave a distinctive partial graph
        // block in the shared allocation. Flush must discard it before queueing.
        for _ in 0..8 {
            let region = playback.acquire(burst).unwrap();
            // SAFETY: the playback credit exclusively owns these sample bytes.
            unsafe {
                std::slice::from_raw_parts_mut(
                    region.pointer,
                    region.frames as usize * channels * 2,
                )
                .fill(0);
            }
            playback.submit(region.frames).unwrap();
            primed += region.frames;
        }
        playback.control(PAUSE).unwrap();
        playback.control(FLUSH).unwrap();
        playback.control(START).unwrap();
        assert_eq!(playback.position().unwrap().discontinuities, 1);
        let region = playback.acquire(burst).unwrap();
        // SAFETY: the exclusively held region has room for all these PCM16 samples.
        unsafe {
            std::slice::from_raw_parts_mut(
                region.pointer.cast::<i16>(),
                region.frames as usize * channels,
            )
            .fill(30_000);
        }
        playback.submit(region.frames).unwrap();
        primed += region.frames;
        playback.control(PAUSE).unwrap();
        playback.control(FLUSH).unwrap();
        playback.control(START).unwrap();
        assert_eq!(playback.position().unwrap().discontinuities, 2);
    }
    capture.control(START).unwrap();
    let capture_thread = thread::spawn(move || {
        let mut tagged = 0;
        let mut total = 0;
        let mut previous = vec![None; channels];
        let mut silence_run = 0;
        let mut interior_silence = 0;
        while total < 48_000 {
            let region = capture.acquire(1024).unwrap();
            // SAFETY: capture retains this read-only pool slot through submit.
            let samples = unsafe {
                std::slice::from_raw_parts(
                    region.pointer.cast::<i16>(),
                    region.frames as usize * channels,
                )
            };
            for (index, &sample) in samples.iter().enumerate() {
                let channel = index % channels;
                let base = 1000 + channel as i16 * 6000;
                if sample == 0 {
                    if tagged > 0 {
                        silence_run += 1;
                    }
                    previous[channel] = None;
                    continue;
                }
                interior_silence += silence_run;
                silence_run = 0;
                assert!(
                    (base..base + 4093).contains(&sample),
                    "corrupt sample or channel order: {sample}"
                );
                if let Some(previous) = previous[channel] {
                    assert_eq!(
                        sample,
                        base + (previous - base + 1) % 4093,
                        "tagged sample order changed or a slot was reused early"
                    );
                }
                previous[channel] = Some(sample);
                tagged += 1;
            }
            total += region.frames;
            capture.submit(region.frames).unwrap();
        }
        capture.control(STANDBY).unwrap();
        (tagged / channels, interior_silence)
    });
    let mut submitted = 0;
    while submitted < 48_000 {
        let region = playback.acquire(burst).unwrap();
        assert_eq!(
            region.frames, burst,
            "graph suggestion truncated a producer burst"
        );
        // SAFETY: playback owns this dedicated slot through submit.
        unsafe {
            let samples = std::slice::from_raw_parts_mut(
                region.pointer.cast::<i16>(),
                region.frames as usize * channels,
            );
            for (offset, sample) in samples.iter_mut().enumerate() {
                *sample = 1000
                    + (offset % channels) as i16 * 6000
                    + ((submitted as usize + offset / channels) % 4093) as i16;
            }
        }
        let frames = region.frames.min(48_000 - submitted);
        playback.submit(frames).unwrap();
        submitted += frames;
    }
    let position = playback.position().unwrap();
    assert!(position.monotonic_ns > 0 && position.frames <= u64::from(submitted + primed));
    playback.control(DRAIN).unwrap();
    let (tagged, interior_silence) = capture_thread.join().unwrap();
    assert_eq!(
        interior_silence, 0,
        "silence inserted within continuous playback"
    );
    assert!(
        tagged > 44_000,
        "loopback did not preserve tagged samples: {tagged}"
    );
    playback.control(STANDBY).unwrap();
    drop(playback);
    assert!(play_worker.join().unwrap().is_err());
    assert!(capture_worker.join().unwrap().is_err());
}

#[test]
fn capture_is_rejected_before_allocating_or_connecting_pipewire() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audio.sock");
    let listener = SeqPacketListener::bind(&path).unwrap();
    // SAFETY: reads the test process identity.
    let options = Options {
        peer_uid: unsafe { libc::geteuid() },
        allow_capture: false,
        target: None,
        remote: None,
        capture_sink: false,
    };
    let worker = thread::spawn(move || serve_connection(listener.accept().unwrap(), &options));
    let result = Client::connect(
        &path,
        Format {
            direction: Direction::Capture,
            rate: RATE,
            channels: 1,
            frames: 1024,
        },
    );
    assert_eq!(result.err().unwrap().raw_os_error(), Some(libc::EACCES));
    assert_eq!(
        worker.join().unwrap().unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
}

#[test]
fn partial_capture_keeps_the_same_buffer_until_consumed() {
    let pipewire = PipeWire::new();
    let (mut capture, worker) = pipewire.connect(
        &pipewire.directory.path().join("partial.sock"),
        Direction::Capture,
    );
    pipewire.link(&capture.node_name(), false);
    capture.control(START).unwrap();
    let first = capture.acquire(1).unwrap();
    capture.submit(0).unwrap();
    let retry = capture.acquire(1).unwrap();
    assert_eq!(
        first.pointer, retry.pointer,
        "zero consumption must retain samples"
    );
    capture.submit(1).unwrap();
    let next = capture.acquire(1).unwrap();
    assert_eq!(
        next.pointer.addr(),
        first.pointer.addr() + 2,
        "short reads must advance within the retained capture buffer"
    );
    capture.submit(1).unwrap();
    capture.control(PAUSE).unwrap();
    capture.control(FLUSH).unwrap();
    capture.control(START).unwrap();
    let next = capture.acquire(128).unwrap();
    capture.submit(next.frames).unwrap();
    assert_eq!(
        capture.position().unwrap().frames,
        2 + u64::from(next.frames)
    );
    capture.control(STANDBY).unwrap();
    drop(capture);
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn stale_generations_and_unowned_credits_close_the_connection() {
    let pipewire = PipeWire::new();
    for stale in [false, true] {
        let path = pipewire
            .directory
            .path()
            .join(format!("invalid-{stale}.sock"));
        let listener = SeqPacketListener::bind(&path).unwrap();
        // SAFETY: reads test credentials.
        let options = Options {
            peer_uid: unsafe { libc::geteuid() },
            allow_capture: false,
            target: None,
            remote: Some(pipewire.remote().to_str().unwrap().into()),
            capture_sink: false,
        };
        let worker = thread::spawn(move || serve_connection(listener.accept().unwrap(), &options));
        let socket = SeqPacket::connect(&path).unwrap();
        socket_timeout(&socket, 3000).unwrap();
        let open = Record {
            op: OPEN,
            request: 1,
            generation: 0,
            values: [1, 48000, 1, 1024, 0],
        };
        socket.send_record(&open.encode(), &[]).unwrap();
        let mut bytes = [0; RECORD_BYTES];
        let config = Record::decode(socket.receive_record_into(&mut bytes).unwrap().bytes).unwrap();
        let invalid = Record {
            op: SUBMIT,
            request: 2,
            generation: config.generation ^ u64::from(stale),
            values: [0, 128, 0, 0, 0],
        };
        socket.send_record(&invalid.encode(), &[]).unwrap();
        let error = Record::decode(socket.receive_record_into(&mut bytes).unwrap().bytes).unwrap();
        assert_eq!(error.op, ERROR);
        assert!(worker.join().unwrap().is_err());
        assert!(socket.receive_record_into(&mut bytes).is_err());
    }
}

#[test]
fn peer_uid_is_checked_before_pipewire_is_opened() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("wrong-uid.sock");
    let listener = SeqPacketListener::bind(&path).unwrap();
    // SAFETY: reads test credentials; select a different UID.
    let options = Options {
        peer_uid: unsafe { libc::geteuid() } ^ 1,
        allow_capture: true,
        target: None,
        remote: None,
        capture_sink: false,
    };
    let worker = thread::spawn(move || serve_connection(listener.accept().unwrap(), &options));
    assert!(
        Client::connect(
            &path,
            Format {
                direction: Direction::Playback,
                rate: RATE,
                channels: 1,
                frames: 1024
            }
        )
        .is_err()
    );
    assert_eq!(
        worker.join().unwrap().unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
}

#[test]
fn broker_accepts_systemd_socket_activation() {
    let directory = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("activated.sock");
    let log_path = directory.path().join("activation.log");
    let log = std::fs::File::create(&log_path).unwrap();
    // The helper sets systemd's LISTEN_PID/FDS and execs the broker with FD 3.
    // No user units or desktop services are started.
    let mut child = Command::new("systemd-socket-activate")
        .args(["--seqpacket", "--now", "--listen"])
        .arg(&path)
        .arg(env!("CARGO_BIN_EXE_droidloom-audio"))
        .arg("--socket")
        .arg(&path)
        .arg("--peer-uid")
        // SAFETY: reads test credentials.
        .arg(unsafe { libc::geteuid() }.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() && child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let result = Client::connect(
        &path,
        Format {
            direction: Direction::Capture,
            rate: RATE,
            channels: 1,
            frames: 1024,
        },
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        result.err().unwrap().raw_os_error(),
        Some(libc::EACCES),
        "activation log: {}",
        std::fs::read_to_string(log_path).unwrap()
    );
}

#[test]
fn pipewire_death_invalidates_the_stream_generation() {
    let mut pipewire = PipeWire::new();
    let (mut playback, worker) = pipewire.connect(
        &pipewire.directory.path().join("disconnect.sock"),
        Direction::Playback,
    );
    pipewire.link(&playback.node_name(), true);
    playback.control(START).unwrap();
    let region = playback.acquire(128).unwrap();
    // SAFETY: exclusively retained playback slot.
    unsafe {
        std::slice::from_raw_parts_mut(region.pointer, region.frames as usize * 2).fill(0);
    }
    playback.submit(region.frames).unwrap();
    pipewire.child.kill().unwrap();
    pipewire.child.wait().unwrap();
    // Acquisition can consume a locally available slot while the disconnect
    // event is pending, so wait for the bounded stream operation to observe it.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match playback.acquire(128) {
            Err(_) => break,
            Ok(region) => {
                if playback.submit(region.frames).is_err() {
                    break;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "dead backend kept accepting samples"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(playback);
    assert!(worker.join().unwrap().is_err());
}
