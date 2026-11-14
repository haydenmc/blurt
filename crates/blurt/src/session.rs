//! One connection to a server, with its audio engine, run as a stream of
//! events for the UI. Voice never passes through the UI: packets go from
//! the connection straight to the engine.

use std::fmt;
use std::sync::Arc;

use blurt_audio::{Closed, Engine, EngineConfig, IncomingVoice, VoiceSource, Waker};
use iced::futures::{SinkExt, Stream};
use mumble_protocol::client::{
    ConnectionOptions, Fingerprint, MumbleClient, MumbleConnectionError, connect,
};

pub struct Request {
    pub options: ConnectionOptions,
    pub engine: EngineConfig,
}

/// A live connection: what the UI needs to act on it.
#[derive(Clone)]
pub struct Session {
    pub client: MumbleClient,
    pub engine: Arc<Engine>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("voice_over_udp", &self.client.is_voice_over_udp())
            .finish_non_exhaustive()
    }
}

impl Session {
    pub fn set_muted(&self, muted: bool) {
        // Silence first; the server hears about it after.
        self.engine.set_muted(muted);
        if let Err(e) = self.client.set_self_mute(muted) {
            log::warn!("Could not change mute: {e}");
        }
    }

    /// Deafening also mutes, as in the official client. Undeafening leaves
    /// the mute as it was asked for before.
    pub fn set_deafened(&self, deafened: bool, muted: bool) {
        self.engine.set_deafened(deafened);
        self.engine.set_muted(deafened || muted);
        if let Err(e) = self.client.set_self_deaf(deafened) {
            log::warn!("Could not change deafen: {e}");
        }
        if !deafened && !muted {
            self.set_muted(false);
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Connected(Session),
    /// The server's certificate isn't trusted yet: ask, then connect again
    /// with it pinned.
    Untrusted {
        fingerprint: Fingerprint,
        reason: String,
    },
    /// Users, channels or chat changed; read the client's state again.
    Changed,
    /// The connection ended, or never started. `None` when we hung up.
    Ended(Option<String>),
}

pub fn run(request: Request) -> impl Stream<Item = Event> {
    iced::stream::channel(64, async move |mut events| {
        let ended = session(request, &mut events).await;
        let _ = events.send(Event::Ended(ended.err())).await;
    })
}

async fn session(
    request: Request,
    events: &mut iced::futures::channel::mpsc::Sender<Event>,
) -> Result<(), String> {
    let mut connection = match connect(request.options).await {
        Ok(connection) => connection,
        Err(MumbleConnectionError::UntrustedCertificate {
            fingerprint,
            reason,
        }) => {
            let _ = events
                .send(Event::Untrusted {
                    fingerprint,
                    reason,
                })
                .await;
            return Ok(());
        }
        Err(e) => return Err(e.to_string()),
    };
    let client = connection.client.clone();

    let (voice, start_forwarding) = forward_voice(connection.audio);
    let sink = {
        let client = client.clone();
        move |opus: &[u8], frame_number, is_terminator| {
            if let Err(e) = client.send_audio(opus, frame_number, is_terminator) {
                log::debug!("Could not send voice: {e}");
            }
        }
    };
    // Opening devices can take a moment; not on the async threads.
    let config = request.engine;
    let engine = tokio::task::spawn_blocking(move || Engine::start(config, voice, sink))
        .await
        .map_err(|e| e.to_string())?;
    let engine = match engine {
        Ok(engine) => Arc::new(engine),
        Err(e) => {
            client.disconnect();
            return Err(format!("Audio: {e}"));
        }
    };
    start_forwarding(engine.waker());

    let session = Session {
        client: client.clone(),
        engine,
    };
    if events.send(Event::Connected(session)).await.is_err() {
        client.disconnect();
        return Ok(());
    }

    while connection.events.recv().await.is_some() {
        // Speaking changes come in bursts; one redraw covers them all.
        while connection.events.try_recv().is_ok() {}
        if events.send(Event::Changed).await.is_err() {
            client.disconnect();
            return Ok(());
        }
    }
    use mumble_protocol::state::ConnectionStatus;
    match *client.state().lock().unwrap().connection_status() {
        ConnectionStatus::Failed => Err("Lost the connection to the server".into()),
        _ => Ok(()),
    }
}

/// Voice packets go to the engine through a bounded queue, waking it for
/// each one so it's played without waiting for the next device callback.
fn forward_voice(
    mut from: tokio::sync::mpsc::Receiver<mumble_protocol::raw_udp::Audio>,
) -> (NetworkVoice, impl FnOnce(Waker)) {
    // A second of 10 ms packets from a few speakers; more means the engine
    // has stopped reading.
    let (tx, rx) = tokio::sync::mpsc::channel(512);
    let start = move |waker: Waker| {
        tokio::spawn(async move {
            while let Some(audio) = from.recv().await {
                if tx.try_send(audio).is_err() {
                    log::debug!("Audio engine isn't keeping up; dropped a voice packet");
                }
                waker.wake();
            }
            // Dropping `tx` closes the engine's source, which stops it.
        });
    };
    (NetworkVoice(rx), start)
}

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
