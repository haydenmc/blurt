//! A headless client for exercising the audio engine against a real server.
//!
//! ```text
//! blurt-cli devices
//! blurt-cli [OPTIONS] HOST             talk and listen
//! blurt-cli measure [OPTIONS] HOST     measure latency through the server
//! ```

use std::process::ExitCode;
use std::time::{Duration, Instant};

use blurt_audio::capture::{Capture, Processing, TransmitMode};
use blurt_audio::codec::EncoderConfig;
use blurt_audio::jitter::JitterConfig;
use blurt_audio::mixer::Mixer;
use blurt_audio::{Closed, Engine, EngineConfig, FRAME_SAMPLES, IncomingVoice, VoiceSource};
use mumble_protocol::client::{
    Connection, ConnectionOptions, MumbleClient, MumbleConnectionError, connect,
};

const USAGE: &str = "\
Usage:
  blurt-cli devices [--audio-host NAME]
  blurt-cli [OPTIONS] HOST            talk and listen
  blurt-cli measure [OPTIONS] HOST    measure latency through the server (no devices used)

Options:
  --port PORT          server port (default 64738)
  --user NAME          username (default blurt-cli)
  --password PASS      server password
  --channel NAME       join this channel
  --trust              trust the server's certificate, whatever it is
  --mode MODE          vad (default) or continuous
  --frames N           10 ms frames per packet: 1 (default) or 2
  --device-buffer N    device buffer in frames at the device rate (default 240)
  --no-aec --no-ns --no-agc
                       turn off echo cancellation, noise suppression, gain
  --loopback           send our voice back to ourselves through the server
  --audio-host NAME    PipeWire, PulseAudio, ALSA... (default: the preferred one;
                       see `blurt-cli devices`)
  --input ID --output ID
                       devices to use (see `blurt-cli devices`)
  --stats              print engine stats every second
  --muted --deafened   start muted (nothing sent) or deafened (nothing played)
  --duration SECS      quit after this long";

struct Args {
    command: Command,
    host: String,
    port: Option<u16>,
    user: String,
    password: Option<String>,
    channel: Option<String>,
    trust: bool,
    loopback: bool,
    stats: bool,
    muted: bool,
    deafened: bool,
    duration: Option<Duration>,
    engine: EngineConfig,
}

#[derive(PartialEq)]
enum Command {
    Devices,
    Talk,
    Measure,
}

fn parse_args() -> Result<Args, lexopt::Error> {
    use lexopt::prelude::*;
    let mut args = Args {
        command: Command::Talk,
        host: String::new(),
        port: None,
        user: "blurt-cli".into(),
        password: None,
        channel: None,
        trust: false,
        loopback: false,
        stats: false,
        muted: false,
        deafened: false,
        duration: None,
        engine: EngineConfig::default(),
    };
    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next()? {
        match arg {
            Long("port") => args.port = Some(parser.value()?.parse()?),
            Long("user") => args.user = parser.value()?.string()?,
            Long("password") => args.password = Some(parser.value()?.string()?),
            Long("channel") => args.channel = Some(parser.value()?.string()?),
            Long("trust") => args.trust = true,
            Long("loopback") => args.loopback = true,
            Long("stats") => args.stats = true,
            Long("muted") => args.muted = true,
            Long("deafened") => args.deafened = true,
            Long("duration") => {
                args.duration = Some(Duration::from_secs_f64(parser.value()?.parse()?))
            }
            Long("mode") => {
                args.engine.mode = match parser.value()?.string()?.as_str() {
                    "vad" => TransmitMode::default(),
                    "continuous" => TransmitMode::Continuous,
                    other => return Err(format!("unknown mode {other:?}").into()),
                }
            }
            Long("frames") => args.engine.encoder.frames_per_packet = parser.value()?.parse()?,
            Long("device-buffer") => args.engine.device_buffer_frames = parser.value()?.parse()?,
            Long("no-aec") => args.engine.processing.echo_cancellation = false,
            Long("no-ns") => args.engine.processing.noise_suppression = false,
            Long("no-agc") => args.engine.processing.auto_gain = false,
            Long("input") => args.engine.input_device = Some(parser.value()?.string()?),
            Long("output") => args.engine.output_device = Some(parser.value()?.string()?),
            Long("audio-host") => args.engine.host = Some(parser.value()?.string()?),
            Long("help") | Short('h') => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            Value(value) if args.host.is_empty() && args.command == Command::Talk => {
                let value = value.string()?;
                match value.as_str() {
                    "devices" => args.command = Command::Devices,
                    "measure" => args.command = Command::Measure,
                    _ => args.host = value,
                }
            }
            Value(value) if args.host.is_empty() => args.host = value.string()?,
            _ => return Err(arg.unexpected()),
        }
    }
    if args.host.is_empty() && args.command != Command::Devices {
        return Err("missing HOST".into());
    }
    Ok(args)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let result = match args.command {
        Command::Devices => list_devices(args.engine.host.as_deref()),
        Command::Talk => talk(args).await,
        Command::Measure => measure(args).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

fn list_devices(host: Option<&str>) -> Result<()> {
    println!("Audio hosts: {}", blurt_audio::hosts().join(", "));
    for (kind, devices) in [
        ("Inputs", blurt_audio::input_devices(host)?),
        ("Outputs", blurt_audio::output_devices(host)?),
    ] {
        println!("{kind}:");
        for device in devices {
            let default = if device.is_default { " (default)" } else { "" };
            println!("  {}{default}\n      {}", device.name, device.id);
        }
    }
    Ok(())
}

async fn open(args: &Args) -> Result<Connection> {
    let options = ConnectionOptions {
        server: args.host.clone(),
        port: args.port,
        username: Some(args.user.clone()),
        password: args.password.clone(),
        client_release: Some(concat!("blurt-cli ", env!("CARGO_PKG_VERSION")).into()),
        ..Default::default()
    };
    let connection = match connect(options.clone()).await {
        Err(MumbleConnectionError::UntrustedCertificate { fingerprint, .. }) if args.trust => {
            log::warn!("Trusting certificate {fingerprint} as asked");
            connect(ConnectionOptions {
                pinned_certificate: Some(fingerprint),
                ..options
            })
            .await?
        }
        Err(MumbleConnectionError::UntrustedCertificate {
            fingerprint,
            reason,
        }) => {
            return Err(format!(
                "untrusted server certificate ({reason}):\n  {fingerprint}\n\
                 Run again with --trust to accept it."
            )
            .into());
        }
        other => other?,
    };

    if let Some(name) = &args.channel {
        let id = connection
            .client
            .state()
            .lock()
            .unwrap()
            .channels()
            .iter()
            .find(|c| &c.name == name)
            .map(|c| c.channel_id)
            .ok_or_else(|| format!("no channel named {name:?}"))?;
        connection.client.set_channel(id)?;
    }

    // Voice starts on the TCP tunnel; give UDP a moment to come up.
    let start = Instant::now();
    while !connection.client.is_voice_over_udp() && start.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    log::info!(
        "Connected; voice over {}",
        if connection.client.is_voice_over_udp() {
            "UDP"
        } else {
            "the TCP tunnel"
        }
    );
    Ok(connection)
}

/// Received voice, straight from the connection.
struct NetworkVoice(tokio::sync::mpsc::Receiver<mumble_protocol::raw_udp::Audio>);

impl VoiceSource for NetworkVoice {
    fn try_recv(&mut self) -> Result<Option<IncomingVoice>, Closed> {
        use tokio::sync::mpsc::error::TryRecvError;
        match self.0.try_recv() {
            Ok(audio) => Ok(Some(IncomingVoice {
                session: audio.sender_session,
                frame_number: audio.frame_number,
                opus: audio.opus_data,
                is_terminator: audio.is_terminator,
            })),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(Closed),
        }
    }
}

/// Sends encoded voice to the server.
fn network_sink(
    client: MumbleClient,
    loopback: bool,
) -> impl FnMut(&[u8], u64, bool) + Send + 'static {
    let target = if loopback {
        MumbleClient::TARGET_LOOPBACK
    } else {
        MumbleClient::TARGET_NORMAL
    };
    move |opus, frame_number, is_terminator| {
        if let Err(e) = client.send_audio_to(opus, frame_number, is_terminator, target) {
            log::debug!("Could not send voice: {e}");
        }
    }
}

async fn talk(args: Args) -> Result<()> {
    let connection = open(&args).await?;
    let client = connection.client.clone();
    let engine = Engine::start(
        args.engine.clone(),
        NetworkVoice(connection.audio),
        network_sink(client.clone(), args.loopback),
    )?;
    engine.set_muted(args.muted);
    engine.set_deafened(args.deafened);
    if args.muted {
        client.set_self_mute(true)?;
    }
    if args.deafened {
        client.set_self_deaf(true)?;
    }
    log::info!("Audio running; Ctrl-C to quit");
    let started = Instant::now();

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = ticker.tick() => {
                if args.duration.is_some_and(|d| started.elapsed() >= d) {
                    break;
                }
                if engine.is_finished() {
                    log::info!("Audio stopped");
                    break;
                }
                if args.stats {
                    let s = engine.stats();
                    println!(
                        "mic {:6.1} dBFS  speech {:.2}  {}  speakers {}  latency in {:4.1} ms / out {:4.1} ms  \
                         tick max {:5.2} ms  overruns {} underruns {}  {}",
                        s.mic_level_db,
                        s.speech,
                        if s.transmitting { "TX" } else { "  " },
                        s.speakers,
                        ms(s.input_latency),
                        ms(s.output_latency),
                        ms(s.max_tick),
                        s.input_overruns,
                        s.output_underruns,
                        if client.is_voice_over_udp() { "udp" } else { "tcp" },
                    );
                }
            }
        }
    }
    drop(engine);
    client.disconnect();
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Measures how long voice takes to go through the encoder, the server,
/// the jitter buffer and the decoder, by sending ourselves clicks with the
/// server's loopback target. Runs in real time, but uses no audio devices,
/// so device latency (shown by `--stats`) comes on top.
async fn measure(args: Args) -> Result<()> {
    const CLICKS: usize = 10;
    const CLICK_EVERY: usize = 50; // frames: half a second

    let connection = open(&args).await?;
    let client = connection.client.clone();
    let mut network = NetworkVoice(connection.audio);
    let config = args.engine.clone();

    let latencies = tokio::task::spawn_blocking(move || -> Result<Vec<Duration>, String> {
        let mut capture = Capture::new(
            // Processing would eat the synthetic click.
            Processing {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain: false,
            },
            TransmitMode::Continuous,
            EncoderConfig {
                frames_per_packet: config.encoder.frames_per_packet,
                ..Default::default()
            },
        )
        .map_err(|e| e.to_string())?;
        let mut mixer = Mixer::new(JitterConfig::default());
        let mut sink = network_sink(client, true);

        let click: [f32; FRAME_SAMPLES] = std::array::from_fn(|i| {
            0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin()
        });
        let silence = [0.0; FRAME_SAMPLES];
        let mut latencies = Vec::new();
        let mut sent_at: Option<Instant> = None;
        let mut heard_quiet = true;
        let start = Instant::now();

        // Up to 60 s, one 10 ms frame per tick, like a sound card.
        for n in 0..6000usize {
            let deadline = start + Duration::from_millis(10 * n as u64);
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));

            // A click every half second, after a second to settle.
            let is_click = n >= 100 && n % CLICK_EVERY == 0 && sent_at.is_none();
            capture.capture(
                if is_click { &click } else { &silence },
                false,
                false,
                &mut sink,
            );
            if is_click {
                sent_at = Some(Instant::now());
            }

            loop {
                match network.try_recv() {
                    Ok(Some(voice)) => mixer.push(voice, Instant::now()),
                    Ok(None) => break,
                    Err(Closed) => return Err("disconnected".into()),
                }
            }
            let mut played = [0.0; FRAME_SAMPLES];
            mixer.mix(&mut played);
            let loud = blurt_audio::capture::level_db(&played) > -20.0;
            if loud
                && heard_quiet
                && let Some(sent) = sent_at.take()
            {
                latencies.push(sent.elapsed());
                if latencies.len() == CLICKS {
                    break;
                }
            }
            heard_quiet = !loud;
        }
        Ok(latencies)
    })
    .await??;

    connection.client.disconnect();
    if latencies.is_empty() {
        return Err("no clicks came back; does the server allow loopback?".into());
    }
    let mut sorted = latencies.clone();
    sorted.sort();
    println!(
        "{} clicks through encoder, server, jitter buffer and decoder:\n  \
         min {:.1} ms, median {:.1} ms, max {:.1} ms\n\
         (10 ms resolution; add device latency from `--stats`)",
        latencies.len(),
        ms(sorted[0]),
        ms(sorted[sorted.len() / 2]),
        ms(sorted[sorted.len() - 1]),
    );
    Ok(())
}
