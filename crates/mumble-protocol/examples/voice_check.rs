//! Connects two clients to a real server and checks that voice gets from one
//! to the other, and over which transport.
//!
//! ```bash
//! cargo run -p mumble-protocol --example voice_check -- HOST [PORT] [PASSWORD]
//! ```

use std::time::{Duration, Instant};

use mumble_protocol::client::{Connection, ConnectionOptions, MumbleConnectionError, connect};

const PACKETS: u64 = 50;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let host = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "localhost".to_string());
    let port = args
        .get(2)
        .map(|p| p.parse().expect("PORT must be a number"));
    let password = args.get(3).cloned();
    let options = |name: &str| ConnectionOptions {
        server: host.clone(),
        port,
        password: password.clone(),
        username: Some(name.to_string()),
        ..Default::default()
    };

    let talker = connect_trusting(options("blurt-talker")).await;
    let mut listener = connect_trusting(options("blurt-listener")).await;

    let start = Instant::now();
    while !(talker.client.is_voice_over_udp() && listener.client.is_voice_over_udp())
        && start.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!(
        "voice over UDP after {:?}: talker {}, listener {}",
        start.elapsed(),
        talker.client.is_voice_over_udp(),
        listener.client.is_voice_over_udp()
    );

    // An Opus packet the server forwards as-is; it does not inspect voice.
    let frame = [0xf8, 0xff, 0xfe];
    for i in 0..PACKETS {
        talker
            .client
            .send_audio(&frame, i, i == PACKETS - 1)
            .expect("send voice");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut received = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while received < PACKETS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, listener.audio.recv()).await {
            Ok(Some(_)) => received += 1,
            _ => break,
        }
    }
    println!("listener received {received}/{PACKETS} voice packets");

    talker.client.disconnect();
    listener.client.disconnect();
    if received < PACKETS {
        std::process::exit(1);
    }
}

/// Connects, trusting whatever certificate the server presents (test servers
/// are self-signed). A real client asks the user first.
async fn connect_trusting(options: ConnectionOptions) -> Connection {
    match connect(options.clone()).await {
        Ok(connection) => connection,
        Err(MumbleConnectionError::UntrustedCertificate { fingerprint, .. }) => {
            connect(ConnectionOptions {
                pinned_certificate: Some(fingerprint),
                ..options
            })
            .await
            .expect("connects with the certificate pinned")
        }
        Err(e) => panic!("connect failed: {e}"),
    }
}
