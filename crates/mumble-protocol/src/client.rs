use futures_util::{Sink, SinkExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use prost::Message;
use serde::Serialize;
use std::io;
use std::net::SocketAddr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc};
use tokio_util::{
    codec::{FramedRead, FramedWrite},
    sync::CancellationToken,
};

mod tls;
pub use tls::{ClientIdentity, Fingerprint};

use crate::udp::{Datagram, UdpVoice};
use crate::voice::VoiceFormat;
use crate::{
    ServerToClientMessage, ServerToClientUdpMessage, UdpMessageId,
    state::{ConnectionStatus, MumbleState},
};

#[derive(Clone)]
pub struct MumbleClient {
    // TODO: Exposed via helper methods?
    state: Arc<Mutex<MumbleState>>,
    cancel: CancellationToken,

    outgoing: mpsc::UnboundedSender<crate::ClientToServerMessage>,

    // Shared across clones so the audio loop sees mute changes immediately,
    // without waiting for the server to echo our UserState back.
    self_muted: Arc<AtomicBool>,
    self_deafened: Arc<AtomicBool>,

    // Whether voice currently goes over UDP (vs. the TCP tunnel).
    voice_over_udp: Arc<AtomicBool>,
}

/// Self-mute as the user sees it: what they asked for, and whether the server
/// has confirmed it yet. Audio stops as soon as a mute is requested, so
/// `Muting` is already silent; it just isn't visible to other users yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SelfMuteStatus {
    Unmuted,
    Muting,
    Muted,
    Unmuting,
}

pub struct Connection {
    pub client: MumbleClient,

    // All incoming events! For debug.
    pub debug: broadcast::Receiver<ServerToClientMessage>,

    // Audio packets, ready for decoding.
    pub audio: mpsc::Receiver<crate::raw_udp::Audio>,

    // General server events, like users joining, leaving, & updating.
    pub events: mpsc::UnboundedReceiver<crate::state::MumbleEvent>,

    /// The server's certificate, so the app can show it or pin it for next
    /// time. `None` only when not connected over TLS (in tests).
    pub server_certificate: Option<Fingerprint>,
}

#[derive(Debug, Error)]
pub enum MumbleConnectionError {
    #[error("Invalid DNS name: {0}")]
    InvalidDnsError(#[from] rustls::pki_types::InvalidDnsNameError),

    #[error("IO error: {0}")]
    IoError(#[from] io::Error),

    #[error("TLS error: {0}")]
    TlsError(#[from] rustls::Error),

    /// The server's certificate is neither CA-signed nor the pinned one. Ask
    /// the user whether to trust `fingerprint`, then retry with it pinned.
    #[error("Untrusted server certificate {fingerprint} ({reason})")]
    UntrustedCertificate {
        fingerprint: Fingerprint,
        reason: String,
    },

    #[error("Unexpected server close")]
    UnexpectedServerClose,

    #[error("Disconnected from server")]
    Disconnected,

    #[error("The server did not answer in time")]
    Timeout,

    #[error("Prost decode error: {0}")]
    ProstDecodeError(#[from] prost::DecodeError),

    #[error("Prost encode error: {0}")]
    ProstEncodeError(#[from] prost::EncodeError),
}

#[derive(Clone, Default)]
pub struct ConnectionOptions {
    // URL to Server, e.g. "mywebsite.chat"
    pub server: String,

    // Port to connect to; defaults to 64738
    pub port: Option<u16>,

    // Server password, if required
    pub password: Option<String>,

    // Username to use
    pub username: Option<String>,

    /// The server certificate the user chose to trust last time, if any.
    /// CA-signed certificates are trusted without one.
    pub pinned_certificate: Option<Fingerprint>,

    /// The client certificate to authenticate with. Registered users are
    /// recognized by it; without one, the user can't register.
    pub identity: Option<ClientIdentity>,

    /// Speak the pre-1.5 voice format even to newer servers (announcing
    /// version 1.4). Mainly for testing legacy support against a 1.5 server.
    pub legacy_voice: bool,

    /// Passwords for password-protected channels.
    pub access_tokens: Vec<String>,

    /// How we describe ourselves to the server (shown in its user info),
    /// e.g. "blurt 0.1.0".
    pub client_release: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerStats {
    pub round_trip_time: u64,
    pub server_version_v2: u64,
    pub user_count: u32,
    pub max_user_count: u32,
    pub max_bandwidth_per_user: u32,
}

/// Asks a server for its version, user count, and bandwidth limit over UDP,
/// without connecting (as server lists do). Works with servers before and
/// after 1.5. UDP may be dropped, so this resends once and gives up after 3 s.
pub async fn get_server_stats(
    server: String,
    port: Option<u16>,
) -> Result<ServerStats, MumbleConnectionError> {
    let port = port.unwrap_or(64738);
    let addr = tokio::net::lookup_host((server.as_str(), port))
        .await?
        .next()
        .ok_or(MumbleConnectionError::IoError(io::Error::other(
            "No address found",
        )))?;
    log::trace!("Pinging server {} on port {}", server, port);
    ping_server(addr).await
}

async fn ping_server(addr: SocketAddr) -> Result<ServerStats, MumbleConnectionError> {
    const RESEND_AFTER: Duration = Duration::from_secs(1);
    const GIVE_UP_AFTER: Duration = Duration::from_secs(3);

    let bind = if addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(addr).await?;

    // Both reply formats echo this back, which identifies our ping.
    let ident = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64);
    // Every server answers the legacy ping: four zero bytes, then the ident.
    let mut legacy = [0u8; 12];
    legacy[4..].copy_from_slice(&ident.to_be_bytes());
    // 1.5+ servers also answer the protobuf one.
    let mut protobuf = vec![crate::raw_udp::Ping::TYPE_ID];
    crate::raw_udp::Ping {
        timestamp: ident,
        request_extended_information: true,
        ..Default::default()
    }
    .encode(&mut protobuf)?;

    let start = tokio::time::Instant::now();
    let deadline = start + GIVE_UP_AFTER;
    let mut next_send = start;
    let mut last_sent = start;
    let mut buf = [0u8; 1024];
    // A 1.5+ server answers both pings, and only its protobuf answer has the
    // exact version (the legacy one caps the patch number at 255). Having the
    // legacy answer, wait this long for the protobuf one.
    const PROTOBUF_GRACE: Duration = Duration::from_millis(250);
    let mut legacy_answer: Option<(ServerStats, tokio::time::Instant)> = None;
    loop {
        if legacy_answer.is_none() && tokio::time::Instant::now() >= next_send {
            last_sent = tokio::time::Instant::now();
            socket.send(&legacy).await?;
            socket.send(&protobuf).await?;
            next_send += RESEND_AFTER;
        }
        let wake = match &legacy_answer {
            Some((_, grace_ends)) => *grace_ends,
            None => next_send.min(deadline),
        };
        let len = match tokio::time::timeout_at(wake, socket.recv(&mut buf)).await {
            Ok(received) => received?,
            Err(_) => {
                if let Some((stats, _)) = legacy_answer {
                    return Ok(stats);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(MumbleConnectionError::Timeout);
                }
                continue;
            }
        };
        let Some((mut stats, is_legacy)) = parse_server_ping(&buf[..len], ident) else {
            continue;
        };
        stats.round_trip_time = last_sent.elapsed().as_millis() as u64;
        if !is_legacy {
            return Ok(stats);
        }
        if stats.server_version_v2 < VoiceFormat::PROTOBUF_SINCE {
            // An old server: no protobuf answer is coming.
            return Ok(stats);
        }
        legacy_answer.get_or_insert((stats, tokio::time::Instant::now() + PROTOBUF_GRACE));
    }
}

/// Parses an answer to either ping format, if it answers ours. Also returns
/// whether it was the legacy format.
fn parse_server_ping(reply: &[u8], ident: u64) -> Option<(ServerStats, bool)> {
    // Legacy: version, ident, users, max users, bandwidth (big-endian).
    if reply.len() == 24 && reply[4..12] == ident.to_be_bytes() {
        let be = |i: usize| u32::from_be_bytes(reply[i..i + 4].try_into().unwrap());
        let v1 = be(0);
        let stats = ServerStats {
            round_trip_time: 0,
            server_version_v2: u64::from(v1 >> 16 & 0xFF) << 48
                | u64::from(v1 >> 8 & 0xFF) << 32
                | u64::from(v1 & 0xFF) << 16,
            user_count: be(12),
            max_user_count: be(16),
            max_bandwidth_per_user: be(20),
        };
        return Some((stats, true));
    }
    let (&kind, body) = reply.split_first()?;
    if kind != crate::raw_udp::Ping::TYPE_ID {
        return None;
    }
    let ping = crate::raw_udp::Ping::decode(body).ok()?;
    let stats = ServerStats {
        round_trip_time: 0,
        server_version_v2: ping.server_version_v2,
        user_count: ping.user_count,
        max_user_count: ping.max_user_count,
        max_bandwidth_per_user: ping.max_bandwidth_per_user,
    };
    (ping.timestamp == ident).then_some((stats, false))
}

pub async fn connect(options: ConnectionOptions) -> Result<Connection, MumbleConnectionError> {
    // Connection process:
    //
    // https://github.com/mumble-voip/mumble/blob/master/docs/dev/network-protocol/establishing_connection.md
    let server = &options.server;
    let port = options.port.unwrap_or(64738);
    log::info!("Connecting to server {} on port {}", server, port);

    let server_name = rustls::pki_types::ServerName::try_from(server.clone())?;
    let tcp = tokio::net::TcpStream::connect((server.clone(), port)).await?;
    // Low latency matters more than fewer packets: voice may be tunnelled here.
    tcp.set_nodelay(true)?;
    // Voice goes over UDP to the same address the TCP connection reached.
    let udp_server = tcp.peer_addr()?;

    let tls = tls::make_connector(options.pinned_certificate, options.identity.as_ref())?;
    let stream = match tls.connector.connect(server_name, tcp).await {
        Ok(stream) => stream,
        Err(e) => {
            let seen = *tls.seen.lock().unwrap();
            return Err(match (tls::certificate_error(&e), seen) {
                (Some(reason), Some(fingerprint)) => MumbleConnectionError::UntrustedCertificate {
                    fingerprint,
                    reason: format!("{reason:?}"),
                },
                _ => e.into(),
            });
        }
    };
    let server_certificate = *tls.seen.lock().unwrap();

    let mut connection = connect_over(stream, options, Some(udp_server)).await?;
    connection.server_certificate = server_certificate;
    Ok(connection)
}

/// Runs the Mumble handshake and starts the connection over an already-open
/// stream: TLS in production, an in-memory pipe in tests. Without a
/// `udp_server`, voice only goes through the TCP tunnel.
async fn connect_over(
    stream: impl AsyncRead + AsyncWrite + Send + 'static,
    options: ConnectionOptions,
    udp_server: Option<SocketAddr>,
) -> Result<Connection, MumbleConnectionError> {
    const DEBUG_CHANNEL_CAPACITY: usize = 512;
    let (debug_tx, debug_rx) = broadcast::channel::<ServerToClientMessage>(DEBUG_CHANNEL_CAPACITY);

    let (stream_read, stream_write) = tokio::io::split(stream);
    let mut reader =
        FramedRead::new(stream_read, crate::codec::MumbleCodec::default()).inspect(move |msg| {
            if let Ok(msg) = msg {
                if let ServerToClientMessage::UdpTunnel(ServerToClientUdpMessage::Audio(_)) = msg {
                    // Audio messages are noisy.
                    // log::trace!("Received audio message: {:?}", msg);
                } else if let ServerToClientMessage::Ping(_) = msg {
                    // Pings are verbose.
                    log::trace!("Received server message: {:?}", msg);
                } else {
                    log::debug!("Received server message: {:?}", msg);
                }
                if debug_tx.receiver_count() > 0 {
                    let _ = debug_tx.send(msg.clone());
                }
            }
        });
    let mut writer = FramedWrite::new(stream_write, crate::codec::MumbleCodec::default());

    // Send a version message! The version tells the server which voice
    // format we speak; announcing 1.4 makes even a 1.5 server use legacy.
    let (major, minor, patch) = if options.legacy_voice {
        (1u32, 4u32, 0u32)
    } else {
        (1u32, 5u32, 0u32)
    };
    let our_version: u32 = (major << 16) | (minor << 8) | patch;
    let our_version_v2: u64 =
        (u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16);

    let version = crate::raw::Version {
        // not necessary, but nice
        os: Some(std::env::consts::OS.into()),
        release: Some(
            options
                .client_release
                .clone()
                .unwrap_or_else(|| concat!("mumble-protocol ", env!("CARGO_PKG_VERSION")).into()),
        ),
        os_version: None,

        // required
        version_v1: Some(our_version),
        version_v2: Some(our_version_v2),
    };
    writer
        .send(crate::ClientToServerMessage::Version(version))
        .await?;
    log::info!("Version message sent to server");

    // Wait for the server's version message.
    //
    // ```
    // [2026-10-15][01:44:44][INFO][mumbo_lib] Received version message: Version
    // { version_v1: Some(67071), version_v2: Some(281496510595072), release:
    // Some("1.5.901"), os: Some("Linux"), os_version: Some("Ubuntu 24.04.4 LTS
    // [arm64]") }
    // ```
    let Some(crate::ServerToClientMessage::Version(version)) = reader.try_next().await? else {
        return Err(MumbleConnectionError::UnexpectedServerClose);
    };
    log::info!("Received version message: {:?}", version);
    let voice_format = if options.legacy_voice {
        VoiceFormat::Legacy
    } else {
        VoiceFormat::for_server(&version)
    };
    log::info!("Using the {voice_format:?} voice format");

    let authenticate = crate::raw::Authenticate {
        username: options.username,
        password: options.password,

        tokens: options.access_tokens,

        // Outdated params
        celt_versions: vec![],
        opus: Some(true),
        client_type: Some(0),
    };
    writer
        .send(crate::ClientToServerMessage::Authenticate(authenticate))
        .await?;
    log::info!("Authenticate message sent to server");

    let state = Arc::new(Mutex::new(MumbleState::default()));

    // Unbounded: an event must never be dropped (a lost UserRemove leaves a
    // ghost user), and the driver must never wait on the app (that would stall
    // pings). Events are infrequent, so the queue stays small.
    let (events_tx, events_rx) = mpsc::unbounded_channel();

    // Assume 20ms packets, let's buffer ~10 seconds worth of audio.
    const AUDIO_CHANNEL_BUFFER_SIZE: usize = 512;
    // Checked at compile time.
    const _: () = assert!(AUDIO_CHANNEL_BUFFER_SIZE > (10 * 1000 / 20));
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(AUDIO_CHANNEL_BUFFER_SIZE);

    // Let's wait for the handshake to complete. The server sends UDP keys
    // (CryptSetup) during it; keep them for the run loop.
    let mut crypt_setup = None;
    while state.lock().unwrap().connection_status() == &ConnectionStatus::Connecting {
        let Some(message) = reader.try_next().await? else {
            return Err(MumbleConnectionError::UnexpectedServerClose);
        };
        if let ServerToClientMessage::CryptSetup(setup) = message {
            crypt_setup = Some(setup);
            continue;
        }
        apply_and_publish(&state, &events_tx, message)?;
    }

    let (server_tx, server_rx) = mpsc::unbounded_channel::<crate::ClientToServerMessage>();

    let cancellation_token = CancellationToken::new();
    let voice_over_udp = Arc::new(AtomicBool::new(false));
    let channels = Channels {
        state: state.clone(),
        audio_tx,
        events_tx,
    };
    let udp = Udp {
        voice: udp_server.map(|server| UdpVoice::new(server, voice_format)),
        format: voice_format,
        initial_setup: crypt_setup,
        active: voice_over_udp.clone(),
    };
    tokio::spawn(
        run(
            reader,
            writer,
            channels,
            udp,
            server_rx,
            cancellation_token.clone(),
        )
        .inspect_err(|e| log::error!("connection ended: {e}")),
    );

    Ok(Connection {
        client: MumbleClient {
            state,
            cancel: cancellation_token,
            outgoing: server_tx,
            self_muted: Arc::new(AtomicBool::new(false)),
            self_deafened: Arc::new(AtomicBool::new(false)),
            voice_over_udp,
        },
        events: events_rx,
        debug: debug_rx,
        audio: audio_rx,
        server_certificate: None,
    })
}

/// Applies a message to the state and publishes any resulting event.
fn apply_and_publish(
    state: &Mutex<MumbleState>,
    events: &mpsc::UnboundedSender<crate::state::MumbleEvent>,
    message: ServerToClientMessage,
) -> Result<(), MumbleConnectionError> {
    let event = state.lock().unwrap().apply(message)?;
    if let Some(event) = event {
        // Only fails if the app dropped its receiver, i.e. nobody is listening.
        let _ = events.send(event);
    }
    Ok(())
}

/// Where the run loop delivers what it receives.
struct Channels {
    state: Arc<Mutex<MumbleState>>,
    audio_tx: tokio::sync::mpsc::Sender<crate::raw_udp::Audio>,
    events_tx: mpsc::UnboundedSender<crate::state::MumbleEvent>,
}

impl Channels {
    /// Handles a message from the server, whether it came over TCP or UDP.
    fn deliver(&self, message: ServerToClientMessage) {
        // Voice goes to the audio task to be played; the state only
        // tracks who is speaking.
        if let ServerToClientMessage::UdpTunnel(ServerToClientUdpMessage::Audio(m)) = &message
            && let Err(e) = self.audio_tx.try_send(m.clone())
        {
            log::error!("Dropping voice packet: {e}");
        }
        // One bad update (e.g. a UserState for an unknown channel) must not
        // end the call; the handshake is where errors are fatal.
        if let Err(e) = apply_and_publish(&self.state, &self.events_tx, message) {
            log::warn!("Ignoring server message that could not be applied: {e}");
        }
    }
}

/// The run loop's UDP voice transport.
struct Udp {
    /// `None` when there is no UDP server address (in tests).
    voice: Option<UdpVoice>,
    /// The voice format, also used for voice tunnelled over TCP.
    format: VoiceFormat,
    /// Keys the server sent during the handshake.
    initial_setup: Option<crate::raw::CryptSetup>,
    /// Shared with `MumbleClient`, so the app can show the transport.
    active: Arc<AtomicBool>,
}

/// Receives on the UDP socket, or waits forever if there isn't one yet.
async fn recv_udp(socket: Option<&UdpSocket>, buf: &mut [u8]) -> io::Result<usize> {
    match socket {
        Some(socket) => socket.recv(buf).await,
        None => std::future::pending().await,
    }
}

/// How long someone can go without sending voice before they're shown as no
/// longer speaking, when no terminator says so. Voice normally arrives every
/// 10-60ms while someone talks.
const SPEAKING_TIMEOUT: Duration = Duration::from_millis(300);
const SPEAKING_CHECK_INTERVAL: Duration = Duration::from_millis(100);

async fn run(
    mut reader: impl Stream<Item = Result<ServerToClientMessage, io::Error>> + Unpin + Send + 'static,
    mut writer: impl Sink<crate::ClientToServerMessage, Error = io::Error> + Unpin + Send + 'static,
    channels: Channels,
    udp: Udp,
    mut server_rx: mpsc::UnboundedReceiver<crate::ClientToServerMessage>,
    cancellation_token: CancellationToken,
) -> Result<(), MumbleConnectionError> {
    let Channels {
        ref state,
        ref events_tx,
        ..
    } = channels;
    let Udp {
        voice: mut udp,
        format: voice_format,
        initial_setup,
        active: udp_active,
    } = udp;
    let mut tunnel_buf = Vec::new();

    // Mumble requires pings every 30s by default; be eager.
    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));
    // UdpVoice decides when to actually ping; this only needs to tick faster
    // than its probe interval.
    let mut udp_tick = tokio::time::interval(Duration::from_millis(250));
    udp_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Ends speaking for anyone whose terminator never arrived.
    let mut speaking_tick = tokio::time::interval(SPEAKING_CHECK_INTERVAL);
    speaking_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut udp_buf = vec![0u8; crate::udp::MAX_DATAGRAM];
    let epoch = Instant::now();

    if let (Some(udp), Some(setup)) = (&mut udp, initial_setup) {
        match udp.on_crypt_setup(&setup, Instant::now()).await {
            Ok(Some(reply)) => {
                writer
                    .send(crate::ClientToServerMessage::CryptSetup(reply))
                    .await?
            }
            Ok(None) => {}
            Err(e) => log::warn!("UDP voice unavailable: {e}"),
        }
    }

    log::info!("Starting client run loop");
    loop {
        udp_active.store(
            udp.as_ref().is_some_and(UdpVoice::is_active),
            Ordering::Relaxed,
        );
        let udp_socket = udp.as_ref().and_then(UdpVoice::socket);

        tokio::select! {
            _ = cancellation_token.cancelled() => {
                log::info!("Cancellation requested, shutting down client loop");
                break;
            }
            message = reader.try_next() => {
                let Some(message) = message? else { break }; // None -> server closed

                if let ServerToClientMessage::CryptSetup(setup) = &message {
                    if let Some(udp) = &mut udp {
                        match udp.on_crypt_setup(setup, Instant::now()).await {
                            Ok(Some(reply)) => writer.send(crate::ClientToServerMessage::CryptSetup(reply)).await?,
                            Ok(None) => {}
                            Err(e) => log::warn!("UDP voice unavailable: {e}"),
                        }
                    }
                    continue;
                }
                channels.deliver(message);
            }
            received = recv_udp(udp_socket.as_deref(), &mut udp_buf) => {
                let Some(udp) = &mut udp else { continue };
                match received {
                    Ok(len) => match udp.on_datagram(&udp_buf[..len], Instant::now()) {
                        Datagram::Message(message) => channels.deliver(ServerToClientMessage::UdpTunnel(message)),
                        Datagram::RequestResync(setup) => writer.send(crate::ClientToServerMessage::CryptSetup(setup)).await?,
                        Datagram::Nothing => {}
                    },
                    // E.g. ECONNREFUSED after an ICMP port-unreachable: UDP
                    // goes quiet and voice falls back to the tunnel.
                    Err(e) => log::debug!("UDP receive failed: {e}"),
                }
            }
            _ = udp_tick.tick(), if udp.is_some() => {
                if let Some(udp) = &mut udp {
                    udp.tick(Instant::now()).await;
                }
            }
            message = server_rx.recv() => {
                if let Some(message) = message {
                    let sent_over_udp = match (&message, &mut udp) {
                        (crate::ClientToServerMessage::UdpTunnel(crate::ClientToServerUdpMessage::Audio(audio)), Some(udp)) => {
                            udp.send_audio(audio).await
                        }
                        _ => false,
                    };
                    if !sent_over_udp {
                        match &message {
                            // Legacy servers expect legacy voice in the tunnel too.
                            crate::ClientToServerMessage::UdpTunnel(crate::ClientToServerUdpMessage::Audio(audio))
                                if voice_format == VoiceFormat::Legacy =>
                            {
                                voice_format.encode_audio(audio, &mut tunnel_buf);
                                writer.send(crate::ClientToServerMessage::UdpTunnel(
                                    crate::ClientToServerUdpMessage::Encoded(tunnel_buf.clone()),
                                )).await?;
                            }
                            crate::ClientToServerMessage::UdpTunnel(_) => writer.send(message.clone()).await?,
                            _ => {
                                log::trace!("Sending message to server: {:?}", message);
                                writer.send(message.clone()).await?;
                            }
                        }
                    }

                    // Handle any bookkeeping needed for specific messages.
                    match &message {
                        crate::ClientToServerMessage::UdpTunnel(crate::ClientToServerUdpMessage::Audio(m)) => {
                            // The server doesn't echo our voice, so track our own speaking.
                            let mut state = state.lock().unwrap();
                            let me = state.current_user_session;
                            if let Some(event) = me.and_then(|me| state.set_speaking(me, !m.is_terminator)) {
                                // Only fails if nobody is listening.
                                let _ = events_tx.send(event);
                            }
                        }
                        crate::ClientToServerMessage::TextMessage(m) => {
                            // Add the message to the local chat history.
                            let mut state = state.lock().unwrap();
                            let user_id = state.current_user_session;
                            state.chat_messages.push(crate::state::TextMessage::ChatTextMessage(crate::state::ChatTextMessage {
                                timestamp: chrono::Utc::now(),
                                actor: user_id,
                                session: m.session.clone(),
                                content: m.message.clone(),
                            }));
                            // Only fails if nobody is listening.
                            let _ = events_tx.send(crate::state::MumbleEvent::ChatMessage);
                        }
                        _ => {
                            // No-op
                        }
                    }
                } else {
                    log::info!("Server channel closed, disconnecting");
                    break; // Channel closed, disconnect
                }
            }
            _ = speaking_tick.tick() => {
                let event = state.lock().unwrap().expire_speaking(Instant::now(), SPEAKING_TIMEOUT);
                if let Some(event) = event {
                    // Only fails if nobody is listening.
                    let _ = events_tx.send(event);
                }
            }
            _ = ping_interval.tick() => {
                log::trace!("Sending ping to server");
                // The server shows these UDP counters in its connection stats.
                let stats = udp.as_ref().map(UdpVoice::stats).unwrap_or_default();
                let ping = crate::raw::Ping {
                    timestamp: Some(u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)),
                    good: Some(stats.good),
                    late: Some(stats.late),
                    lost: Some(stats.lost),
                    resync: Some(stats.resync),
                    udp_ping_avg: udp.as_ref().and_then(UdpVoice::rtt).map(|rtt| rtt.as_secs_f32() * 1000.0),
                    ..Default::default()
                };
                writer.send(crate::ClientToServerMessage::Ping(ping)).await?;
            }
        }
    }
    log::info!("Disconnected from server");
    Ok(())
}

impl MumbleClient {
    pub fn disconnect(&self) {
        self.cancel.cancel();
    }

    pub fn state(&self) -> Arc<Mutex<MumbleState>> {
        self.state.clone()
    }

    /// Whether voice currently goes over UDP. It starts on the TCP tunnel and
    /// moves to UDP once the server answers a UDP ping.
    pub fn is_voice_over_udp(&self) -> bool {
        self.voice_over_udp.load(Ordering::Relaxed)
    }

    /// Sends one Opus packet. Set `is_terminator` on the last packet of a
    /// burst so listeners know the speaker stopped rather than lost packets.
    pub fn send_audio(
        &self,
        audio: &[u8],
        frame_counter: u64,
        is_terminator: bool,
    ) -> Result<(), MumbleConnectionError> {
        self.send_audio_to(audio, frame_counter, is_terminator, Self::TARGET_NORMAL)
    }

    /// Normal speech, to everyone in our channel.
    pub const TARGET_NORMAL: u32 = 0;
    /// The server sends our voice straight back to us (for testing).
    pub const TARGET_LOOPBACK: u32 = 31;

    /// Like [`send_audio`](Self::send_audio), but to a voice target: one set up
    /// with [`set_voice_target`](Self::set_voice_target) (1–30), or one of the
    /// `TARGET_*` constants.
    pub fn send_audio_to(
        &self,
        audio: &[u8],
        frame_counter: u64,
        is_terminator: bool,
        target: u32,
    ) -> Result<(), MumbleConnectionError> {
        let muted = self.self_muted.load(Ordering::Relaxed);
        if muted && !is_terminator {
            return Ok(());
        }
        // Muting mid-burst must still end the burst, or listeners (and our own
        // state) would keep us marked as speaking; send it without any voice.
        let audio = if muted { &[] } else { audio };
        let packet = crate::ClientToServerMessage::UdpTunnel(
            crate::ClientToServerUdpMessage::Audio(crate::raw_udp::Audio {
                opus_data: audio.to_vec(),
                header: Some(crate::raw_udp::audio::Header::Target(target)),
                frame_number: frame_counter,
                is_terminator,

                ..Default::default()
            }),
        );
        self.send(packet)
    }

    /// Sends a text message to the specified channel.
    ///
    /// Defaults to the current user's channel.
    pub fn send_channel_message(
        &self,
        message: String,
        channel_id: Option<u32>,
    ) -> Result<(), MumbleConnectionError> {
        let channel_id = if let Some(channel_id) = channel_id {
            channel_id
        } else {
            self.state
                .lock()
                .unwrap()
                .current_user()
                .ok_or(MumbleConnectionError::Disconnected)?
                .channel_id
        };
        log::info!(
            "Sending text message: {} to channel: {:?}",
            message,
            channel_id
        );
        self.send(crate::ClientToServerMessage::TextMessage(
            crate::raw::TextMessage {
                message: message.clone(),
                channel_id: vec![channel_id],
                ..Default::default()
            },
        ))?;
        Ok(())
    }

    /// Send a DM to one or more users specified by their session IDs.
    pub fn send_user_message(
        &self,
        message: String,
        session: Vec<u32>,
    ) -> Result<(), MumbleConnectionError> {
        log::info!("Sending text message: {} to user: {:?}", message, session);
        self.send(crate::ClientToServerMessage::TextMessage(
            crate::raw::TextMessage {
                message: message.clone(),
                session,
                ..Default::default()
            },
        ))?;
        Ok(())
    }

    /// Mutes or unmutes the microphone: stops sending audio and tells the
    /// server (and so other users) that we are self-muted.
    pub fn set_self_mute(&self, muted: bool) -> Result<(), MumbleConnectionError> {
        self.self_muted.store(muted, Ordering::Relaxed);
        self.send(crate::ClientToServerMessage::UserState(Box::new(
            crate::raw::UserState {
                self_mute: Some(muted),
                ..Default::default()
            },
        )))
    }

    /// Deafens or undeafens us. Deafened, the server sends us no voice.
    /// Deafening also mutes, as in the official client; undeafening leaves
    /// the mute as it is (use [`set_self_mute`](Self::set_self_mute)).
    pub fn set_self_deaf(&self, deafened: bool) -> Result<(), MumbleConnectionError> {
        self.self_deafened.store(deafened, Ordering::Relaxed);
        if deafened {
            self.self_muted.store(true, Ordering::Relaxed);
        }
        self.send(crate::ClientToServerMessage::UserState(Box::new(
            crate::raw::UserState {
                self_deaf: Some(deafened),
                self_mute: deafened.then_some(true),
                ..Default::default()
            },
        )))
    }

    /// Whether we asked to be deafened. Takes effect immediately, before the
    /// server confirms, so the audio engine can stop playback at once.
    pub fn is_self_deafened(&self) -> bool {
        self.self_deafened.load(Ordering::Relaxed)
    }

    /// Whether we asked to be muted (directly, or by deafening).
    pub fn is_self_muted(&self) -> bool {
        self.self_muted.load(Ordering::Relaxed)
    }

    pub fn set_channel(&self, channel_id: u32) -> Result<(), MumbleConnectionError> {
        self.send(crate::ClientToServerMessage::UserState(Box::new(
            crate::raw::UserState {
                channel_id: Some(channel_id),
                ..Default::default()
            },
        )))
    }

    /// Replaces our access tokens: the passwords that let us into
    /// password-protected channels. Send them before joining such a channel.
    pub fn set_access_tokens(&self, tokens: Vec<String>) -> Result<(), MumbleConnectionError> {
        self.send(crate::ClientToServerMessage::Authenticate(
            crate::raw::Authenticate {
                tokens,
                ..Default::default()
            },
        ))
    }

    /// Defines voice target `id` (1–30) for whispering to users or shouting
    /// to channels; then send voice with [`send_audio_to`](Self::send_audio_to).
    pub fn set_voice_target(
        &self,
        id: u32,
        targets: Vec<crate::raw::voice_target::Target>,
    ) -> Result<(), MumbleConnectionError> {
        self.send(crate::ClientToServerMessage::VoiceTarget(
            crate::raw::VoiceTarget {
                id: Some(id),
                targets,
            },
        ))
    }

    /// Asks for our permissions in a channel; the server answers with a
    /// `PermissionQuery`.
    pub fn query_permissions(&self, channel_id: u32) -> Result<(), MumbleConnectionError> {
        self.send(crate::ClientToServerMessage::PermissionQuery(
            crate::raw::PermissionQuery {
                channel_id: Some(channel_id),
                ..Default::default()
            },
        ))
    }

    /// Asks for a user's connection statistics; the server answers with a
    /// `UserStats`. `stats_only` skips the certificate and version details.
    pub fn request_user_stats(
        &self,
        session: u32,
        stats_only: bool,
    ) -> Result<(), MumbleConnectionError> {
        self.send(crate::ClientToServerMessage::UserStats(Box::new(
            crate::raw::UserStats {
                session: Some(session),
                stats_only: Some(stats_only),
                ..Default::default()
            },
        )))
    }

    /// Asks for large fields the server leaves out of state updates and
    /// sends only on request: avatars, comments, and channel descriptions.
    pub fn request_blobs(
        &self,
        request: crate::raw::RequestBlob,
    ) -> Result<(), MumbleConnectionError> {
        self.send(crate::ClientToServerMessage::RequestBlob(request))
    }

    /// Compares the mute we last requested against the one the server last
    /// reported for us. Until the server's `UserState` arrives, the two differ.
    pub fn self_mute_status(&self) -> SelfMuteStatus {
        let requested = self.self_muted.load(Ordering::Relaxed);
        let confirmed = self
            .state
            .lock()
            .unwrap()
            .current_user()
            .is_some_and(|user| user.self_mute);
        match (requested, confirmed) {
            (false, false) => SelfMuteStatus::Unmuted,
            (true, false) => SelfMuteStatus::Muting,
            (true, true) => SelfMuteStatus::Muted,
            (false, true) => SelfMuteStatus::Unmuting,
        }
    }

    /// Asks the server to register us under our current name, tied to our
    /// client certificate. Needs an identity and the "Register Self" permission.
    pub fn register_self(&self) -> Result<(), MumbleConnectionError> {
        let session = self
            .state
            .lock()
            .unwrap()
            .current_user_session
            .ok_or(MumbleConnectionError::Disconnected)?;
        self.send(crate::ClientToServerMessage::UserState(Box::new(
            crate::raw::UserState {
                session: Some(session),
                user_id: Some(0),
                ..Default::default()
            },
        )))
    }

    fn send(&self, message: crate::ClientToServerMessage) -> Result<(), MumbleConnectionError> {
        self.outgoing
            .send(message)
            .map_err(|_| MumbleConnectionError::Disconnected)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{MumbleFrame, MumbleFrameCodec};
    use crate::{TcpMessageId, UdpMessageId, raw, raw_udp};
    use prost::Message;
    use std::time::Duration;
    use tokio::io::DuplexStream;
    use tokio_util::codec::Framed;

    /// Generous bound so a broken test fails instead of hanging.
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// The server end of an in-memory connection, speaking raw frames.
    struct FakeServer {
        frames: Framed<DuplexStream, MumbleFrameCodec>,
    }

    impl FakeServer {
        async fn send<M: TcpMessageId>(&mut self, message: M) {
            let frame = MumbleFrame {
                type_id: M::TYPE_ID,
                payload: message.encode_to_vec().into(),
            };
            self.frames.send(frame).await.unwrap();
        }

        async fn send_audio(&mut self, audio: raw_udp::Audio) {
            // Tunnelled voice: one UDP type byte, then the protobuf.
            let mut payload = vec![raw_udp::Audio::TYPE_ID];
            payload.extend(audio.encode_to_vec());
            self.frames
                .send(MumbleFrame {
                    type_id: raw::UdpTunnel::TYPE_ID,
                    payload: payload.into(),
                })
                .await
                .unwrap();
        }

        /// The next frame the client sent, or `None` once it hung up.
        async fn receive(&mut self) -> Option<MumbleFrame> {
            tokio::time::timeout(TIMEOUT, self.frames.next())
                .await
                .unwrap()
                .map(Result::unwrap)
        }

        async fn receive_message<M: TcpMessageId>(&mut self) -> M {
            loop {
                let frame = self.receive().await.expect("client hung up");
                // Keepalives arrive whenever; they're never what a test is waiting for.
                if frame.type_id == raw::Ping::TYPE_ID && M::TYPE_ID != raw::Ping::TYPE_ID {
                    continue;
                }
                assert_eq!(frame.type_id, M::TYPE_ID, "unexpected message type");
                return M::decode(frame.payload).unwrap();
            }
        }

        /// The next voice packet the client sent, skipping pings.
        async fn receive_audio(&mut self) -> raw_udp::Audio {
            loop {
                let frame = self.receive().await.expect("client hung up");
                if frame.type_id == raw::Ping::TYPE_ID {
                    continue;
                }
                assert_eq!(
                    frame.type_id,
                    raw::UdpTunnel::TYPE_ID,
                    "unexpected message type"
                );
                assert_eq!(
                    frame.payload[0],
                    raw_udp::Audio::TYPE_ID,
                    "not a voice packet"
                );
                return raw_udp::Audio::decode(&frame.payload[1..]).unwrap();
            }
        }

        /// Reads the client's opening Version and Authenticate, and answers with Version.
        async fn greet(&mut self) -> raw::Authenticate {
            self.receive_message::<raw::Version>().await;
            self.send(raw::Version {
                version_v2: Some(1 << 48 | 5 << 32),
                ..Default::default()
            })
            .await;
            self.receive_message::<raw::Authenticate>().await
        }

        /// Completes the handshake with one other user already on the server.
        async fn accept(&mut self) {
            self.greet().await;
            self.send(user(1, "Alice")).await;
            self.send(user(2, "me")).await;
            self.send(raw::ServerSync {
                session: Some(2),
                ..Default::default()
            })
            .await;
        }
    }

    fn user(session: u32, name: &str) -> raw::UserState {
        raw::UserState {
            session: Some(session),
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    fn options() -> ConnectionOptions {
        ConnectionOptions {
            server: "test.invalid".to_string(),
            port: None,
            password: Some("hunter2".to_string()),
            username: Some("me".to_string()),
            ..Default::default()
        }
    }

    /// Starts a client handshake against a fake server.
    fn start() -> (
        tokio::task::JoinHandle<Result<Connection, MumbleConnectionError>>,
        FakeServer,
    ) {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let connecting = tokio::spawn(connect_over(client_end, options(), None));
        (
            connecting,
            FakeServer {
                frames: Framed::new(server_end, MumbleFrameCodec),
            },
        )
    }

    async fn connected() -> (Connection, FakeServer) {
        let (connecting, mut server) = start();
        server.accept().await;
        let connection = tokio::time::timeout(TIMEOUT, connecting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (connection, server)
    }

    /// Names of the users the client thinks are speaking.
    fn speaking(client: &MumbleClient) -> Vec<String> {
        client
            .state()
            .lock()
            .unwrap()
            .users()
            .iter()
            .filter(|u| u.is_speaking)
            .map(|u| u.name.clone())
            .collect()
    }

    /// Discards the events reported so far, e.g. from the handshake.
    fn drain_events(connection: &mut Connection) {
        while connection.events.try_recv().is_ok() {}
    }

    fn user_names(client: &MumbleClient) -> Vec<String> {
        client
            .state()
            .lock()
            .unwrap()
            .users()
            .iter()
            .map(|u| u.name.clone())
            .collect()
    }

    #[tokio::test]
    async fn handshake_sends_version_then_credentials() {
        let (_connecting, mut server) = start();

        let version = server.receive_message::<raw::Version>().await;
        server.send(raw::Version::default()).await;
        let authenticate = server.receive_message::<raw::Authenticate>().await;

        assert_eq!(
            version.version_v2,
            Some(1 << 48 | 5 << 32),
            "should claim 1.5.0"
        );
        assert_eq!(authenticate.username.as_deref(), Some("me"));
        assert_eq!(authenticate.password.as_deref(), Some("hunter2"));
        assert_eq!(
            authenticate.opus,
            Some(true),
            "must advertise Opus or the server may fall back to CELT"
        );
    }

    #[tokio::test]
    async fn connect_keeps_every_user_sent_before_server_sync() {
        let (connection, _server) = connected().await;

        assert_eq!(user_names(&connection.client), ["Alice", "me"]);
    }

    #[tokio::test]
    async fn connect_waits_for_server_sync() {
        let (mut connecting, mut server) = start();
        server.greet().await;
        server.send(user(1, "Alice")).await;

        // Everything but ServerSync has arrived: still connecting.
        let still_waiting = tokio::time::timeout(Duration::from_millis(50), &mut connecting).await;
        assert!(still_waiting.is_err(), "connect returned before ServerSync");

        server
            .send(raw::ServerSync {
                session: Some(2),
                ..Default::default()
            })
            .await;
        tokio::time::timeout(TIMEOUT, connecting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn connect_fails_when_the_server_rejects() {
        let (connecting, mut server) = start();
        server.greet().await;
        server
            .send(raw::Reject {
                r#type: Some(raw::reject::RejectType::WrongServerPw as i32),
                reason: Some("Invalid server password".to_string()),
            })
            .await;

        let error = tokio::time::timeout(TIMEOUT, connecting)
            .await
            .unwrap()
            .unwrap()
            .err()
            .expect("connect should fail");

        assert!(
            error.to_string().contains("Invalid server password"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn connect_fails_when_the_server_hangs_up_mid_handshake() {
        let (connecting, mut server) = start();
        server.greet().await;
        drop(server);

        let error = tokio::time::timeout(TIMEOUT, connecting)
            .await
            .unwrap()
            .unwrap()
            .err()
            .expect("connect should fail");

        assert!(
            matches!(error, MumbleConnectionError::UnexpectedServerClose),
            "{error}"
        );
    }

    #[tokio::test]
    async fn audio_goes_to_the_audio_channel_not_the_state() {
        let (mut connection, mut server) = connected().await;

        server
            .send_audio(raw_udp::Audio {
                sender_session: 1,
                opus_data: vec![1, 2, 3],
                ..Default::default()
            })
            .await;
        let audio = tokio::time::timeout(TIMEOUT, connection.audio.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!((audio.sender_session, audio.opus_data), (1, vec![1, 2, 3]));
        assert_eq!(user_names(&connection.client), ["Alice", "me"]);
    }

    #[tokio::test]
    async fn only_the_last_packet_of_a_burst_is_a_terminator() {
        let (connection, mut server) = connected().await;

        connection.client.send_audio(&[1], 7, false).unwrap();
        connection.client.send_audio(&[2], 8, true).unwrap();
        let first = server.receive_audio().await;
        let last = server.receive_audio().await;

        assert_eq!(
            (first.opus_data, first.frame_number, first.is_terminator),
            (vec![1], 7, false)
        );
        assert_eq!(
            (last.opus_data, last.frame_number, last.is_terminator),
            (vec![2], 8, true)
        );
    }

    #[tokio::test]
    async fn debug_stream_includes_the_handshake() {
        let (mut connection, _server) = connected().await;

        let mut received = Vec::new();
        while let Ok(message) = connection.debug.try_recv() {
            received.push(message);
        }

        assert!(
            matches!(received[0], ServerToClientMessage::Version(_)),
            "{received:?}"
        );
        assert!(
            matches!(received.last(), Some(ServerToClientMessage::ServerSync(_))),
            "{received:?}"
        );
        assert_eq!(
            received.len(),
            4,
            "Version, two UserStates, ServerSync: {received:?}"
        );
    }

    #[tokio::test]
    async fn events_report_the_handshake_and_later_changes() {
        let (mut connection, mut server) = connected().await;

        // Both users from the handshake were reported before connect returned…
        let mut handshake = Vec::new();
        while let Ok(event) = connection.events.try_recv() {
            handshake.push(event);
        }
        assert!(
            matches!(
                handshake[..],
                [
                    crate::state::MumbleEvent::UsersChanged,
                    crate::state::MumbleEvent::UsersChanged
                ]
            ),
            "{handshake:?}"
        );

        // …and changes keep being reported afterwards.
        server
            .send(raw::TextMessage {
                message: "hi".to_string(),
                ..Default::default()
            })
            .await;
        let event = tokio::time::timeout(TIMEOUT, connection.events.recv())
            .await
            .unwrap();
        assert!(
            matches!(event, Some(crate::state::MumbleEvent::ChatMessage)),
            "{event:?}"
        );
    }

    #[tokio::test]
    async fn state_keeps_updating_after_connect() {
        let (connection, mut server) = connected().await;

        server.send(user(3, "Bob")).await;

        wait_until(|| user_names(&connection.client).contains(&"Bob".to_string())).await;
    }

    #[tokio::test]
    async fn bad_or_unknown_messages_do_not_end_the_connection() {
        let (connection, mut server) = connected().await;

        // A message type this crate does not decode...
        server
            .send(raw::UserStats {
                session: Some(1),
                ..Default::default()
            })
            .await;
        // ...a payload that is not valid protobuf...
        server
            .frames
            .send(MumbleFrame {
                type_id: raw::UserState::TYPE_ID,
                payload: vec![0xff, 0xff, 0xff].into(),
            })
            .await
            .unwrap();
        // ...and a valid message the state cannot apply.
        server
            .send(raw::UserState {
                session: Some(1),
                channel_id: Some(999),
                ..Default::default()
            })
            .await;

        server.send(user(3, "Bob")).await;
        wait_until(|| user_names(&connection.client).contains(&"Bob".to_string())).await;
    }

    #[tokio::test]
    async fn disconnect_closes_the_connection() {
        let (mut connection, mut server) = connected().await;

        connection.client.disconnect();

        // The server sees the socket close (after any pings already in flight)…
        while server.receive().await.is_some() {}
        // …and the audio channel closes, which is how the app's audio task stops.
        assert!(
            tokio::time::timeout(TIMEOUT, connection.audio.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn server_hanging_up_closes_the_audio_channel() {
        let (mut connection, server) = connected().await;

        drop(server);

        assert!(
            tokio::time::timeout(TIMEOUT, connection.audio.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pings_every_15_seconds_to_stay_connected() {
        let (_connection, mut server) = connected().await;

        // The first ping goes out as soon as the loop starts.
        assert_eq!(server.receive().await.unwrap().type_id, raw::Ping::TYPE_ID);

        tokio::time::advance(Duration::from_secs(15)).await;
        assert_eq!(server.receive().await.unwrap().type_id, raw::Ping::TYPE_ID);
    }

    #[tokio::test]
    async fn a_full_audio_channel_does_not_stall_the_connection() {
        let (connection, mut server) = connected().await;

        // Nobody reads `connection.audio`; overflow it, then send a state update.
        for _ in 0..600 {
            server
                .send_audio(raw_udp::Audio {
                    sender_session: 1,
                    ..Default::default()
                })
                .await;
        }
        server.send(user(3, "Bob")).await;

        wait_until(|| user_names(&connection.client).contains(&"Bob".to_string())).await;
    }

    #[tokio::test]
    async fn sending_voice_marks_me_as_speaking_until_the_terminator() {
        let (connection, mut server) = connected().await;

        connection.client.send_audio(&[1], 0, false).unwrap();
        server.receive_audio().await;
        wait_until(|| speaking(&connection.client) == ["me"]).await;

        connection.client.send_audio(&[2], 1, true).unwrap();
        server.receive_audio().await;
        wait_until(|| speaking(&connection.client).is_empty()).await;
    }

    #[tokio::test]
    async fn muting_mid_speech_still_ends_speaking() {
        let (connection, mut server) = connected().await;

        connection.client.send_audio(&[1], 0, false).unwrap();
        server.receive_audio().await;
        wait_until(|| speaking(&connection.client) == ["me"]).await;

        connection.client.set_self_mute(true).unwrap();
        connection.client.send_audio(&[2], 1, false).unwrap();
        connection.client.send_audio(&[3], 2, true).unwrap();

        wait_until(|| speaking(&connection.client).is_empty()).await;

        // Packets are ordered, so the first audio after the mute is the
        // terminator only if the ordinary packet was dropped.
        assert_eq!(
            server.receive_message::<raw::UserState>().await.self_mute,
            Some(true)
        );
        let terminator = server.receive_audio().await;
        assert_eq!(
            (
                terminator.opus_data,
                terminator.frame_number,
                terminator.is_terminator
            ),
            (vec![], 2, true)
        );
    }

    #[tokio::test]
    async fn mute_is_pending_until_the_server_confirms() {
        let (connection, mut server) = connected().await;
        let client = &connection.client;
        assert_eq!(client.self_mute_status(), SelfMuteStatus::Unmuted);

        client.set_self_mute(true).unwrap();
        assert_eq!(client.self_mute_status(), SelfMuteStatus::Muting);

        server.receive_message::<raw::UserState>().await;
        server
            .send(raw::UserState {
                session: Some(2),
                self_mute: Some(true),
                ..Default::default()
            })
            .await;
        wait_until(|| client.self_mute_status() == SelfMuteStatus::Muted).await;

        client.set_self_mute(false).unwrap();
        assert_eq!(client.self_mute_status(), SelfMuteStatus::Unmuting);

        server.receive_message::<raw::UserState>().await;
        server
            .send(raw::UserState {
                session: Some(2),
                self_mute: Some(false),
                ..Default::default()
            })
            .await;
        wait_until(|| client.self_mute_status() == SelfMuteStatus::Unmuted).await;
    }

    #[tokio::test]
    async fn sending_voice_is_reported() {
        let (mut connection, mut server) = connected().await;
        drain_events(&mut connection);

        connection.client.send_audio(&[1], 0, false).unwrap();
        server.receive_audio().await;
        let event = tokio::time::timeout(TIMEOUT, connection.events.recv())
            .await
            .unwrap();

        assert!(
            matches!(event, Some(crate::state::MumbleEvent::UsersChanged)),
            "{event:?}"
        );
    }

    #[tokio::test]
    async fn received_voice_is_played_and_marks_the_sender_as_speaking() {
        let (mut connection, mut server) = connected().await;

        server
            .send_audio(raw_udp::Audio {
                sender_session: 1,
                opus_data: vec![1, 2, 3],
                ..Default::default()
            })
            .await;

        let audio = tokio::time::timeout(TIMEOUT, connection.audio.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(audio.opus_data, [1, 2, 3]);
        assert_eq!(speaking(&connection.client), ["Alice"]);
    }

    // See state::tests::continued_speech_is_reported_once.
    #[tokio::test]
    async fn my_continued_speech_is_reported_once() {
        let (mut connection, mut server) = connected().await;
        drain_events(&mut connection);

        for frame in 0..5 {
            connection.client.send_audio(&[1], frame, false).unwrap();
            server.receive_audio().await;
        }
        // Give the client loop a moment to report anything else.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut events = Vec::new();
        while let Ok(event) = connection.events.try_recv() {
            events.push(event);
        }
        assert_eq!(events.len(), 1, "{events:?}");
    }

    const UDP_KEY: [u8; 16] = [3; 16];
    const UDP_CLIENT_NONCE: [u8; 16] = [4; 16];
    const UDP_SERVER_NONCE: [u8; 16] = [5; 16];

    /// The UDP side of a fake server, keyed to match what it tells the client.
    struct FakeUdpServer {
        socket: UdpSocket,
        crypt: crate::crypt::CryptState,
        client: Option<SocketAddr>,
    }

    impl FakeUdpServer {
        async fn new() -> Self {
            Self {
                socket: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
                // The server encrypts with the nonce the client decrypts with.
                crypt: crate::crypt::CryptState::new(
                    &UDP_KEY,
                    &UDP_SERVER_NONCE,
                    &UDP_CLIENT_NONCE,
                )
                .unwrap(),
                client: None,
            }
        }

        fn crypt_setup() -> raw::CryptSetup {
            raw::CryptSetup {
                key: Some(UDP_KEY.to_vec()),
                client_nonce: Some(UDP_CLIENT_NONCE.to_vec()),
                server_nonce: Some(UDP_SERVER_NONCE.to_vec()),
            }
        }

        /// The next datagram from the client, decrypted: its type and body.
        async fn receive(&mut self) -> (u8, Vec<u8>) {
            let mut buf = [0; 2048];
            let (len, from) = tokio::time::timeout(TIMEOUT, self.socket.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            self.client = Some(from);
            let mut plain = vec![0; len - crate::crypt::HEADER_LEN];
            self.crypt.decrypt(&buf[..len], &mut plain).unwrap();
            (plain[0], plain[1..].to_vec())
        }

        /// The next voice packet the client sent over UDP, skipping pings.
        async fn receive_audio(&mut self) -> raw_udp::Audio {
            loop {
                let (kind, body) = self.receive().await;
                if kind == raw_udp::Audio::TYPE_ID {
                    return raw_udp::Audio::decode(body.as_slice()).unwrap();
                }
            }
        }

        async fn send(&mut self, kind: u8, message: &impl Message) {
            let mut plain = vec![kind];
            message.encode(&mut plain).unwrap();
            let mut sealed = vec![0; plain.len() + crate::crypt::HEADER_LEN];
            self.crypt.encrypt(&plain, &mut sealed);
            let client = self.client.expect("the client has not sent anything yet");
            self.socket.send_to(&sealed, client).await.unwrap();
        }
    }

    /// Connects with UDP keys sent during the handshake, as real servers do.
    async fn connected_with_udp(udp: &FakeUdpServer) -> (Connection, FakeServer) {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let udp_addr = udp.socket.local_addr().unwrap();
        let connecting = tokio::spawn(connect_over(client_end, options(), Some(udp_addr)));
        let mut server = FakeServer {
            frames: Framed::new(server_end, MumbleFrameCodec),
        };
        server.greet().await;
        server.send(FakeUdpServer::crypt_setup()).await;
        server.send(user(1, "Alice")).await;
        server.send(user(2, "me")).await;
        server
            .send(raw::ServerSync {
                session: Some(2),
                ..Default::default()
            })
            .await;
        let connection = tokio::time::timeout(TIMEOUT, connecting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (connection, server)
    }

    #[tokio::test]
    async fn voice_moves_to_udp_once_the_server_answers() {
        let mut udp = FakeUdpServer::new().await;
        let (mut connection, mut server) = connected_with_udp(&udp).await;

        // The client pings over UDP; until the server answers, voice is tunnelled.
        let (kind, ping) = udp.receive().await;
        assert_eq!(kind, raw_udp::Ping::TYPE_ID);
        assert!(!connection.client.is_voice_over_udp());
        connection.client.send_audio(&[1], 1, false).unwrap();
        assert_eq!(server.receive_audio().await.frame_number, 1);

        let ping = raw_udp::Ping::decode(ping.as_slice()).unwrap();
        udp.send(raw_udp::Ping::TYPE_ID, &ping).await;
        wait_until(|| connection.client.is_voice_over_udp()).await;

        // Our voice now goes over UDP...
        connection.client.send_audio(&[2], 2, false).unwrap();
        assert_eq!(udp.receive_audio().await.frame_number, 2);

        // ...and so does the server's.
        let audio = raw_udp::Audio {
            sender_session: 1,
            frame_number: 5,
            opus_data: vec![9],
            ..Default::default()
        };
        udp.send(raw_udp::Audio::TYPE_ID, &audio).await;
        let received = tokio::time::timeout(TIMEOUT, connection.audio.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, audio);
        // (We are speaking too: our own burst has not ended.)
        wait_until(|| speaking(&connection.client).contains(&"Alice".to_string())).await;
    }

    #[tokio::test]
    async fn voice_stays_on_tcp_when_udp_gets_no_answer() {
        let mut udp = FakeUdpServer::new().await;
        let (connection, mut server) = connected_with_udp(&udp).await;

        // Pinged, but never answered.
        udp.receive().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(!connection.client.is_voice_over_udp());
        connection.client.send_audio(&[1], 1, false).unwrap();
        assert_eq!(server.receive_audio().await.frame_number, 1);
    }

    #[tokio::test]
    async fn answers_a_nonce_request_over_tcp() {
        let udp = FakeUdpServer::new().await;
        let (_connection, mut server) = connected_with_udp(&udp).await;

        server.send(raw::CryptSetup::default()).await;

        let reply = server.receive_message::<raw::CryptSetup>().await;
        assert!(reply.client_nonce.is_some());
        assert!(reply.key.is_none() && reply.server_nonce.is_none());
    }

    #[tokio::test]
    async fn deafening_also_mutes_and_stops_our_voice() {
        let (connection, mut server) = connected().await;

        connection.client.set_self_deaf(true).unwrap();
        let state = server.receive_message::<raw::UserState>().await;
        assert_eq!(state.self_deaf, Some(true));
        assert_eq!(state.self_mute, Some(true));
        assert!(connection.client.is_self_deafened());
        assert!(connection.client.is_self_muted());

        // Muted, so only the terminator of a burst goes out, without voice.
        connection.client.send_audio(&[1], 1, false).unwrap();
        connection.client.send_audio(&[2], 2, true).unwrap();
        let audio = server.receive_audio().await;
        assert_eq!(audio.frame_number, 2);
        assert!(audio.opus_data.is_empty());

        // Undeafening leaves the mute alone.
        connection.client.set_self_deaf(false).unwrap();
        let state = server.receive_message::<raw::UserState>().await;
        assert_eq!(state.self_deaf, Some(false));
        assert_eq!(state.self_mute, None);
        assert!(connection.client.is_self_muted());
    }

    #[tokio::test]
    async fn access_tokens_are_sent_on_connect() {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let options = ConnectionOptions {
            access_tokens: vec!["sesame".to_string()],
            ..options()
        };
        tokio::spawn(connect_over(client_end, options, None));
        let mut server = FakeServer {
            frames: Framed::new(server_end, MumbleFrameCodec),
        };
        assert_eq!(server.greet().await.tokens, ["sesame"]);
    }

    #[tokio::test]
    async fn access_tokens_can_change_while_connected() {
        let (connection, mut server) = connected().await;

        connection
            .client
            .set_access_tokens(vec!["a".to_string(), "b".to_string()])
            .unwrap();

        let auth = server.receive_message::<raw::Authenticate>().await;
        assert_eq!(auth.tokens, ["a", "b"]);
        assert_eq!(auth.username, None, "only the tokens change");
    }

    #[tokio::test]
    async fn whispering_sets_up_a_target_and_sends_to_it() {
        let (connection, mut server) = connected().await;

        let target = raw::voice_target::Target {
            session: vec![1],
            ..Default::default()
        };
        connection.client.set_voice_target(3, vec![target]).unwrap();
        let voice_target = server.receive_message::<raw::VoiceTarget>().await;
        assert_eq!(voice_target.id, Some(3));
        assert_eq!(voice_target.targets[0].session, [1]);

        connection.client.send_audio_to(&[1], 1, false, 3).unwrap();
        assert_eq!(
            server.receive_audio().await.header,
            Some(raw_udp::audio::Header::Target(3))
        );
    }

    #[test]
    fn legacy_server_ping_replies_parse() {
        let ident = 0x0102_0304_0506_0708u64;
        let mut reply = vec![0, 1, 4, 7]; // Version 1.4.7.
        reply.extend_from_slice(&ident.to_be_bytes());
        for v in [5u32, 100, 72_000] {
            reply.extend_from_slice(&v.to_be_bytes());
        }

        let (stats, is_legacy) = parse_server_ping(&reply, ident).unwrap();
        assert!(is_legacy);
        assert_eq!(stats.server_version_v2, 1 << 48 | 4 << 32 | 7 << 16);
        assert_eq!(stats.user_count, 5);
        assert_eq!(stats.max_user_count, 100);
        assert_eq!(stats.max_bandwidth_per_user, 72_000);
        assert!(parse_server_ping(&reply, ident + 1).is_none(), "not ours");
    }

    #[tokio::test]
    async fn server_ping_works_with_a_legacy_only_server() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let (len, from) = server.recv_from(&mut buf).await.unwrap();
                // An old server ignores the protobuf ping.
                if !(len == 12 && buf[..4] == [0; 4]) {
                    continue;
                }
                let mut reply = vec![0, 1, 4, 0];
                reply.extend_from_slice(&buf[4..12]);
                reply.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 10, 0, 1, 0, 0]);
                server.send_to(&reply, from).await.unwrap();
            }
        });

        let stats = ping_server(addr).await.unwrap();
        assert_eq!(stats.user_count, 2);
        assert_eq!(stats.server_version_v2, 1 << 48 | 4 << 32);
    }

    #[tokio::test]
    async fn server_ping_prefers_the_exact_version_from_newer_servers() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let (len, from) = server.recv_from(&mut buf).await.unwrap();
                if len == 12 && buf[..4] == [0; 4] {
                    // The legacy answer caps the patch number: 1.5.255.
                    let mut reply = vec![0, 1, 5, 255];
                    reply.extend_from_slice(&buf[4..12]);
                    reply.extend_from_slice(&[0; 12]);
                    server.send_to(&reply, from).await.unwrap();
                } else {
                    // The protobuf answer, a little later, has it exactly.
                    let ping = raw_udp::Ping::decode(&buf[1..len]).unwrap();
                    let mut reply = vec![raw_udp::Ping::TYPE_ID];
                    raw_udp::Ping {
                        timestamp: ping.timestamp,
                        server_version_v2: 1 << 48 | 5 << 32 | 735 << 16,
                        ..Default::default()
                    }
                    .encode(&mut reply)
                    .unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    server.send_to(&reply, from).await.unwrap();
                }
            }
        });

        let stats = ping_server(addr).await.unwrap();
        assert_eq!(stats.server_version_v2, 1 << 48 | 5 << 32 | 735 << 16);
    }

    #[tokio::test(start_paused = true)]
    async fn server_ping_gives_up_on_a_silent_server() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let result = ping_server(silent.local_addr().unwrap()).await;
        assert!(matches!(result, Err(MumbleConnectionError::Timeout)));
    }

    async fn wait_until(condition: impl Fn() -> bool) {
        tokio::time::timeout(TIMEOUT, async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition never became true");
    }
}
