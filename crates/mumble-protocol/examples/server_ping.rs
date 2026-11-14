//! Pings a server the way server lists do, without connecting.
//!
//! ```bash
//! cargo run -p mumble-protocol --example server_ping -- HOST [PORT]
//! ```

use mumble_protocol::client::get_server_stats;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let host = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "localhost".to_string());
    let port = args
        .get(2)
        .map(|p| p.parse().expect("PORT must be a number"));

    match get_server_stats(host, port).await {
        Ok(stats) => {
            let v = stats.server_version_v2;
            println!(
                "version {}.{}.{}, {} of {} users, {} bit/s per user, {} ms",
                v >> 48,
                v >> 32 & 0xFFFF,
                v >> 16 & 0xFFFF,
                stats.user_count,
                stats.max_user_count,
                stats.max_bandwidth_per_user,
                stats.round_trip_time
            );
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
