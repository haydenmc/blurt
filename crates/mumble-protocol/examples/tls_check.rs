//! Checks server certificate pinning and client-certificate identity against
//! a real (self-signed) server.
//!
//! ```bash
//! cargo run -p mumble-protocol --example tls_check -- HOST [PORT] [PASSWORD]
//! ```

use std::time::{Duration, Instant};

use mumble_protocol::client::{
    ClientIdentity, Connection, ConnectionOptions, MumbleConnectionError, connect,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let base = ConnectionOptions {
        server: args
            .get(1)
            .cloned()
            .unwrap_or_else(|| "localhost".to_string()),
        port: args
            .get(2)
            .map(|p| p.parse().expect("PORT must be a number")),
        password: args.get(3).cloned(),
        ..Default::default()
    };

    // 1. A self-signed server is refused, with its fingerprint.
    let fingerprint = match connect(named(&base, "blurt-tofu")).await {
        Err(MumbleConnectionError::UntrustedCertificate {
            fingerprint,
            reason,
        }) => {
            println!("untrusted, as expected: {reason}\n  fingerprint {fingerprint}");
            fingerprint
        }
        Ok(_) => panic!("a self-signed certificate was trusted without a pin"),
        Err(e) => panic!("unexpected error: {e}"),
    };

    // 2. Pinned, it is accepted.
    let pinned = ConnectionOptions {
        pinned_certificate: Some(fingerprint),
        ..base
    };
    let connection = connect(named(&pinned, "blurt-tofu"))
        .await
        .expect("pinned connect");
    assert_eq!(connection.server_certificate, Some(fingerprint));
    println!("pinned: connected");
    connection.client.disconnect();

    // 3. A client certificate makes a registration that survives reconnecting.
    let name = format!("blurt-{}", std::process::id());
    let identity = ClientIdentity::generate(&name).expect("generate identity");
    let with_identity = ConnectionOptions {
        identity: Some(identity),
        ..named(&pinned, &name)
    };

    let connection = connect(with_identity.clone()).await.expect("connect");
    assert_eq!(
        my_user_id(&connection),
        None,
        "a new identity is unregistered"
    );
    connection.client.register_self().expect("register");
    let start = Instant::now();
    while my_user_id(&connection).is_none() && start.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let registered = my_user_id(&connection).expect("server registered us");
    println!("registered {name} as user {registered}");
    connection.client.disconnect();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let connection = connect(with_identity).await.expect("reconnect");
    assert_eq!(my_user_id(&connection), Some(registered));
    println!("reconnected: recognized as user {registered}");
    connection.client.disconnect();
}

fn named(base: &ConnectionOptions, name: &str) -> ConnectionOptions {
    ConnectionOptions {
        username: Some(name.to_string()),
        ..base.clone()
    }
}

fn my_user_id(connection: &Connection) -> Option<u32> {
    connection
        .client
        .state()
        .lock()
        .unwrap()
        .current_user()
        .and_then(|me| me.user_id)
}
