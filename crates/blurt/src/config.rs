//! Saved servers, trusted certificates and our client identity, kept as
//! TOML (and DER files) in the platform's config directory.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use mumble_protocol::client::{ClientIdentity, Fingerprint};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub servers: Vec<Server>,
    /// Server certificates the user chose to trust, by `host:port`, as hex
    /// SHA-256 fingerprints.
    pub trusted: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Server {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// The server password, if it has one.
    // TODO: keep it in the OS keyring instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

pub const DEFAULT_PORT: u16 = 64738;

impl Server {
    /// How trusted certificates are keyed.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl Config {
    pub fn trusted_certificate(&self, server: &Server) -> Option<Fingerprint> {
        parse_fingerprint(self.trusted.get(&server.address())?)
    }

    pub fn trust(&mut self, server: &Server, fingerprint: &Fingerprint) {
        self.trusted.insert(server.address(), hex(&fingerprint.0));
    }
}

/// Where the config lives, e.g. `~/.config/blurt` on Linux.
pub fn dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "blurt").map(|d| d.config_dir().to_owned())
}

/// Loads the config, or an empty one if there is none yet.
pub fn load(dir: &Path) -> io::Result<Config> {
    match std::fs::read_to_string(dir.join("config.toml")) {
        Ok(text) => {
            toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e),
    }
}

pub fn save(dir: &Path, config: &Config) -> io::Result<()> {
    let text = toml::to_string_pretty(config).map_err(io::Error::other)?;
    write_private(&dir.join("config.toml"), text.as_bytes())
}

/// Our client certificate, generated on first use. Servers recognize
/// registered users by it, so it has to stay the same between runs.
pub fn identity(dir: &Path, name: &str) -> io::Result<ClientIdentity> {
    let cert = dir.join("identity.cert.der");
    let key = dir.join("identity.key.der");
    match (std::fs::read(&cert), std::fs::read(&key)) {
        (Ok(cert_der), Ok(key_der)) => Ok(ClientIdentity { cert_der, key_der }),
        _ => {
            let identity = ClientIdentity::generate(name).map_err(io::Error::other)?;
            write_private(&cert, &identity.cert_der)?;
            write_private(&key, &identity.key_der)?;
            Ok(identity)
        }
    }
}

/// Writes a file only we can read: it may hold passwords or our key.
/// Goes through a temporary file, so a crash never leaves half a config.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    io::Write::write_all(&mut options.open(&temporary)?, bytes)?;
    std::fs::rename(temporary, path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_fingerprint(text: &str) -> Option<Fingerprint> {
    let text = text.replace(':', "");
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(Fingerprint(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server {
            label: "Friends".into(),
            host: "example.org".into(),
            port: DEFAULT_PORT,
            username: "me".into(),
            password: Some("secret".into()),
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let mut config = Config {
            servers: vec![server()],
            ..Default::default()
        };
        config.trust(&server(), &Fingerprint([0xab; 32]));
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
        assert_eq!(
            config.trusted_certificate(&server()),
            Some(Fingerprint([0xab; 32]))
        );
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("blurt-config-test-{}", std::process::id()));
        assert_eq!(load(&dir).unwrap(), Config::default());
        let config = Config {
            servers: vec![server()],
            ..Default::default()
        };
        save(&dir, &config).unwrap();
        assert_eq!(load(&dir).unwrap(), config);
        let first = identity(&dir, "me").unwrap();
        let again = identity(&dir, "me").unwrap();
        assert_eq!(first.cert_der, again.cert_der);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_bad_fingerprints() {
        assert_eq!(parse_fingerprint("abc"), None);
        assert_eq!(parse_fingerprint(&"zz".repeat(32)), None);
    }
}
