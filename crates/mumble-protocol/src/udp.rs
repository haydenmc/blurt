//! Voice over UDP, with automatic fallback to the TCP tunnel.
//!
//! Once the server sends keys in `CryptSetup`, we open a UDP socket and send
//! encrypted pings. Voice goes over UDP once the server answers one, and goes
//! back to the TCP tunnel if UDP goes quiet (e.g. a firewall drops it). This
//! mirrors the official client's behavior.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use crate::crypt::{CryptState, CryptStats, HEADER_LEN};
use crate::voice::VoiceFormat;
use crate::{ServerToClientUdpMessage, raw, raw_udp};

/// How often to ping while UDP works (the official client uses 5 s).
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// How often to ping while waiting for UDP to start working, so voice moves
/// to UDP within about a second of connecting.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// With no good packet for this long, voice falls back to the TCP tunnel.
const STALE_AFTER: Duration = Duration::from_secs(12);
/// With packets failing to decrypt for this long, ask the server to resync
/// nonces (and don't ask more often than this).
const RESYNC_AFTER: Duration = Duration::from_secs(5);

/// Large enough for any datagram Mumble sends (it caps them at 1024 bytes).
pub(crate) const MAX_DATAGRAM: usize = 2048;

/// What a received datagram turned out to be.
#[derive(Debug)]
pub(crate) enum Datagram {
    /// Voice (or an unsolicited ping), for the app.
    Message(ServerToClientUdpMessage),
    /// Decryption keeps failing: send this `CryptSetup` over TCP to resync.
    RequestResync(raw::CryptSetup),
    /// Handled here (a ping reply), or dropped (undecryptable or malformed).
    Nothing,
}

pub(crate) struct UdpVoice {
    server: SocketAddr,
    format: VoiceFormat,
    socket: Option<Arc<UdpSocket>>,
    crypt: Option<CryptState>,
    /// Whether voice should go over UDP rather than the TCP tunnel.
    active: bool,
    keyed_at: Option<Instant>,
    last_good: Option<Instant>,
    last_ping: Option<Instant>,
    last_resync_request: Option<Instant>,
    rtt: Option<Duration>,
    /// Ping timestamps are microseconds since this instant.
    epoch: Instant,
    plain: Vec<u8>,
    sealed: Vec<u8>,
}

impl UdpVoice {
    pub fn new(server: SocketAddr, format: VoiceFormat) -> Self {
        Self {
            server,
            format,
            socket: None,
            crypt: None,
            active: false,
            keyed_at: None,
            last_good: None,
            last_ping: None,
            last_resync_request: None,
            rtt: None,
            epoch: Instant::now(),
            plain: Vec::with_capacity(MAX_DATAGRAM),
            sealed: Vec::with_capacity(MAX_DATAGRAM),
        }
    }

    /// The socket to receive on, once keys have arrived.
    pub fn socket(&self) -> Option<Arc<UdpSocket>> {
        self.socket.clone()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn stats(&self) -> CryptStats {
        self.crypt.as_ref().map(|c| c.stats).unwrap_or_default()
    }

    /// Round-trip time of the last UDP ping.
    pub fn rtt(&self) -> Option<Duration> {
        self.rtt
    }

    /// Handles the server's `CryptSetup`, returning a reply to send over TCP.
    pub async fn on_crypt_setup(
        &mut self,
        setup: &raw::CryptSetup,
        now: Instant,
    ) -> io::Result<Option<raw::CryptSetup>> {
        match (&setup.key, &setup.client_nonce, &setup.server_nonce) {
            // Full keys: (re)start encryption.
            (Some(key), Some(client_nonce), Some(server_nonce)) => {
                let Some(crypt) = CryptState::new(key, client_nonce, server_nonce) else {
                    log::warn!("Ignoring CryptSetup with malformed keys");
                    return Ok(None);
                };
                self.crypt = Some(crypt);
                self.active = false;
                self.keyed_at = Some(now);
                self.last_good = None;
                if self.socket.is_none() {
                    let bind: SocketAddr = if self.server.is_ipv6() {
                        "[::]:0".parse().unwrap()
                    } else {
                        "0.0.0.0:0".parse().unwrap()
                    };
                    let socket = UdpSocket::bind(bind).await?;
                    socket.connect(self.server).await?;
                    log::info!(
                        "UDP voice socket {} -> {}",
                        socket.local_addr()?,
                        self.server
                    );
                    self.socket = Some(Arc::new(socket));
                }
                self.send_ping(now).await;
                Ok(None)
            }
            // Only the server's nonce: it is resyncing us.
            (None, None, Some(server_nonce)) => {
                if let Some(crypt) = &mut self.crypt
                    && !crypt.set_decrypt_iv(server_nonce)
                {
                    log::warn!("Ignoring CryptSetup with a malformed server nonce");
                }
                Ok(None)
            }
            // Empty: the server wants our nonce.
            (None, None, None) => Ok(self.crypt.as_ref().map(|crypt| raw::CryptSetup {
                client_nonce: Some(crypt.encrypt_iv().to_vec()),
                ..Default::default()
            })),
            _ => {
                log::warn!("Ignoring CryptSetup with an unexpected combination of fields");
                Ok(None)
            }
        }
    }

    /// Periodic upkeep: falls back to TCP if UDP went quiet, and pings.
    pub async fn tick(&mut self, now: Instant) {
        if self.crypt.is_none() {
            return;
        }
        if self.active && self.last_good.is_none_or(|t| now - t > STALE_AFTER) {
            log::warn!("UDP voice went quiet; falling back to the TCP tunnel");
            self.active = false;
        }
        let interval = if self.active {
            PING_INTERVAL
        } else {
            PROBE_INTERVAL
        };
        if self.last_ping.is_none_or(|t| now - t >= interval) {
            self.send_ping(now).await;
        }
    }

    /// Sends voice over UDP if it is working. Returns `false` if the caller
    /// should send it through the TCP tunnel instead.
    pub async fn send_audio(&mut self, audio: &raw_udp::Audio) -> bool {
        if !self.active {
            return false;
        }
        self.format.encode_audio(audio, &mut self.plain);
        self.send_plain().await
    }

    /// Decrypts and decodes one received datagram.
    pub fn on_datagram(&mut self, datagram: &[u8], now: Instant) -> Datagram {
        let Some(crypt) = &mut self.crypt else {
            return Datagram::Nothing;
        };
        if datagram.len() < HEADER_LEN {
            return Datagram::Nothing;
        }

        self.plain.resize(datagram.len() - HEADER_LEN, 0);
        if let Err(e) = crypt.decrypt(datagram, &mut self.plain) {
            log::debug!("Dropping UDP packet: {e}");
            let since_good = self.last_good.or(self.keyed_at).map(|t| now - t);
            let since_request = self.last_resync_request.map(|t| now - t);
            if since_good.is_some_and(|d| d > RESYNC_AFTER)
                && since_request.is_none_or(|d| d > RESYNC_AFTER)
            {
                log::info!("Requesting a UDP nonce resync");
                self.last_resync_request = Some(now);
                return Datagram::RequestResync(raw::CryptSetup::default());
            }
            return Datagram::Nothing;
        }

        self.last_good = Some(now);
        if !self.active {
            log::info!("UDP voice is working");
            self.active = true;
        }

        match crate::voice::decode(&self.plain) {
            Ok(ServerToClientUdpMessage::Ping(ping)) => {
                let sent = self.epoch + Duration::from_micros(ping.timestamp);
                self.rtt = Some(now.saturating_duration_since(sent));
                Datagram::Nothing
            }
            Ok(message) => Datagram::Message(message),
            Err(e) => {
                log::debug!("Dropping UDP packet: {e}");
                Datagram::Nothing
            }
        }
    }

    async fn send_ping(&mut self, now: Instant) {
        self.last_ping = Some(now);
        let timestamp = u64::try_from((now - self.epoch).as_micros()).unwrap_or(u64::MAX);
        self.format.encode_ping(timestamp, &mut self.plain);
        self.send_plain().await;
    }

    /// Encrypts `self.plain` and sends it. Returns whether it was sent.
    async fn send_plain(&mut self) -> bool {
        let (Some(crypt), Some(socket)) = (&mut self.crypt, &self.socket) else {
            return false;
        };
        self.sealed.resize(self.plain.len() + HEADER_LEN, 0);
        crypt.encrypt(&self.plain, &mut self.sealed);
        match socket.send(&self.sealed).await {
            Ok(_) => true,
            Err(e) => {
                // E.g. ECONNREFUSED after an ICMP port-unreachable.
                log::debug!("UDP send failed: {e}");
                self.active = false;
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UdpMessageId;
    use prost::Message;

    const KEY: [u8; 16] = [7; 16];
    const CLIENT_NONCE: [u8; 16] = [1; 16];
    const SERVER_NONCE: [u8; 16] = [2; 16];

    fn keys() -> raw::CryptSetup {
        raw::CryptSetup {
            key: Some(KEY.to_vec()),
            client_nonce: Some(CLIENT_NONCE.to_vec()),
            server_nonce: Some(SERVER_NONCE.to_vec()),
        }
    }

    /// The server's side of the encryption: it encrypts with our decrypt nonce.
    fn server_crypt() -> CryptState {
        CryptState::new(&KEY, &SERVER_NONCE, &CLIENT_NONCE).unwrap()
    }

    fn seal(crypt: &mut CryptState, kind: u8, message: &impl Message) -> Vec<u8> {
        let mut plain = vec![kind];
        message.encode(&mut plain).unwrap();
        let mut sealed = vec![0; plain.len() + HEADER_LEN];
        crypt.encrypt(&plain, &mut sealed);
        sealed
    }

    async fn keyed(now: Instant) -> (UdpVoice, UdpSocket) {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut udp = UdpVoice::new(server.local_addr().unwrap(), VoiceFormat::Protobuf);
        assert!(udp.on_crypt_setup(&keys(), now).await.unwrap().is_none());
        (udp, server)
    }

    #[tokio::test]
    async fn keys_open_a_socket_and_ping_right_away() {
        let (_udp, server) = keyed(Instant::now()).await;

        let mut buf = [0; MAX_DATAGRAM];
        let n = server.recv(&mut buf).await.unwrap();
        let mut crypt = server_crypt();
        let mut plain = vec![0; n - HEADER_LEN];
        crypt.decrypt(&buf[..n], &mut plain).unwrap();
        assert_eq!(plain[0], raw_udp::Ping::TYPE_ID);
    }

    #[tokio::test]
    async fn voice_uses_udp_only_after_the_server_answers() {
        let now = Instant::now();
        let (mut udp, _server) = keyed(now).await;
        let audio = raw_udp::Audio::default();
        assert!(!udp.send_audio(&audio).await, "UDP is unproven");

        let reply = seal(
            &mut server_crypt(),
            raw_udp::Ping::TYPE_ID,
            &raw_udp::Ping::default(),
        );
        assert!(matches!(udp.on_datagram(&reply, now), Datagram::Nothing));
        assert!(udp.is_active());
        assert!(udp.send_audio(&audio).await);
    }

    #[tokio::test]
    async fn received_voice_is_decoded() {
        let now = Instant::now();
        let (mut udp, _server) = keyed(now).await;
        let audio = raw_udp::Audio {
            opus_data: vec![1, 2, 3],
            frame_number: 9,
            ..Default::default()
        };
        let packet = seal(&mut server_crypt(), raw_udp::Audio::TYPE_ID, &audio);

        let Datagram::Message(ServerToClientUdpMessage::Audio(got)) = udp.on_datagram(&packet, now)
        else {
            panic!("expected voice");
        };
        assert_eq!(got, audio);
        assert_eq!(udp.stats().good, 1);
    }

    #[tokio::test]
    async fn falls_back_to_tcp_when_udp_goes_quiet() {
        let start = Instant::now();
        let (mut udp, _server) = keyed(start).await;
        let reply = seal(
            &mut server_crypt(),
            raw_udp::Ping::TYPE_ID,
            &raw_udp::Ping::default(),
        );
        udp.on_datagram(&reply, start);
        assert!(udp.is_active());

        udp.tick(start + STALE_AFTER).await;
        assert!(udp.is_active(), "not stale yet");
        udp.tick(start + STALE_AFTER + Duration::from_millis(1))
            .await;
        assert!(!udp.is_active());
    }

    #[tokio::test]
    async fn persistent_decrypt_failures_request_a_resync_rate_limited() {
        let start = Instant::now();
        let (mut udp, _server) = keyed(start).await;
        // Encrypted with the wrong key, so it never decrypts.
        let mut wrong = CryptState::new(&[9; 16], &SERVER_NONCE, &CLIENT_NONCE).unwrap();
        let mut bad = || {
            seal(
                &mut wrong,
                raw_udp::Ping::TYPE_ID,
                &raw_udp::Ping::default(),
            )
        };

        assert!(
            matches!(udp.on_datagram(&bad(), start), Datagram::Nothing),
            "too soon"
        );
        let later = start + RESYNC_AFTER + Duration::from_millis(1);
        assert!(matches!(
            udp.on_datagram(&bad(), later),
            Datagram::RequestResync(_)
        ));
        assert!(
            matches!(udp.on_datagram(&bad(), later), Datagram::Nothing),
            "rate-limited"
        );
    }

    #[tokio::test]
    async fn empty_crypt_setup_is_answered_with_our_nonce() {
        let (mut udp, _server) = keyed(Instant::now()).await;
        let reply = udp
            .on_crypt_setup(&raw::CryptSetup::default(), Instant::now())
            .await
            .unwrap()
            .unwrap();
        // One packet (the first ping) has advanced our nonce by one.
        let mut expected = CLIENT_NONCE;
        expected[0] += 1;
        assert_eq!(reply.client_nonce.unwrap(), expected);
    }

    #[tokio::test]
    async fn server_nonce_alone_resyncs_decryption() {
        let now = Instant::now();
        let (mut udp, _server) = keyed(now).await;
        let new_nonce = [0x40; 16];
        let setup = raw::CryptSetup {
            server_nonce: Some(new_nonce.to_vec()),
            ..Default::default()
        };
        assert!(udp.on_crypt_setup(&setup, now).await.unwrap().is_none());
        assert_eq!(udp.stats().resync, 1);

        let mut server = CryptState::new(&KEY, &new_nonce, &CLIENT_NONCE).unwrap();
        let packet = seal(
            &mut server,
            raw_udp::Ping::TYPE_ID,
            &raw_udp::Ping::default(),
        );
        udp.on_datagram(&packet, now);
        assert!(udp.is_active());
    }
}
