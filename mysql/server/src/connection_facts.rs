//! What a connection knows about where it came from.

use std::net::SocketAddr;

/// What a connection knows about itself, which `CONNECTION_ID()`, `USER()`,
/// `CURRENT_USER()` and `@@socket` answer.
///
/// A connection the runtime did not accept — one an embedding hands the
/// adapter directly — knows none of it. It listened on no socket, which is
/// what an empty socket says, and the calls asking for the rest are refused
/// there rather than answered with a guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MySqlConnectionFacts {
    connection_id: Option<u32>,
    client_host: Option<String>,
    /// Where the client came from as `SHOW PROCESSLIST` writes it, which is
    /// the host with its port over TCP.
    listed_host: Option<String>,
    unix_socket_path: Option<Vec<u8>>,
    account_name: Option<String>,
}

impl MySqlConnectionFacts {
    /// A connection accepted on the Unix socket at `path`.
    ///
    /// Measured on MySQL 8.4.11, a client on the socket comes from
    /// `localhost`.
    pub(crate) fn on_unix_socket(connection_id: u32, path: Vec<u8>) -> Self {
        assert!(!path.is_empty(), "a Unix socket has a path");
        Self {
            connection_id: Some(connection_id),
            client_host: Some("localhost".to_owned()),
            listed_host: Some("localhost".to_owned()),
            unix_socket_path: Some(path),
            account_name: None,
        }
    }

    /// A connection accepted over TCP from `peer`.
    ///
    /// This server looks no name up, so the client's host is its address, as
    /// MySQL writes one under `skip_name_resolve` — measured on 8.4.11,
    /// `app@172.17.0.9`. An IPv4 address a dual-stack socket reports inside
    /// IPv6 is written as the IPv4 one, as MySQL writes it. A socket that no
    /// longer knows its peer leaves the host unknown.
    ///
    /// Measured on MySQL 8.4.11, `SHOW PROCESSLIST` writes the host with the
    /// client's port after a colon — `172.17.0.3:40964`.
    pub(crate) fn over_tcp(connection_id: u32, peer: Option<SocketAddr>) -> Self {
        let host = peer.map(|peer| peer.ip().to_canonical().to_string());
        Self {
            connection_id: Some(connection_id),
            listed_host: peer
                .zip(host.as_ref())
                .map(|(peer, host)| format!("{host}:{}", peer.port())),
            client_host: host,
            unix_socket_path: None,
            account_name: None,
        }
    }

    /// Names the account the connection logged in as, once it has.
    pub(crate) fn with_account_name(mut self, name: String) -> Self {
        self.account_name = Some(name);
        self
    }

    /// The ID the handshake gave this connection.
    pub(crate) fn connection_id(&self) -> Option<u32> {
        self.connection_id
    }

    /// `USER()`: the name the client logged in with, at the host it came from.
    pub(crate) fn user(&self) -> Option<String> {
        Some(format!(
            "{}@{}",
            self.account_name.as_deref()?,
            self.client_host.as_deref()?
        ))
    }

    /// The account name, the host as `SHOW PROCESSLIST` writes it and the
    /// connection ID, when the connection knows all three.
    pub(crate) fn listed_session(&self) -> Option<(u32, &str, &str)> {
        Some((
            self.connection_id?,
            self.account_name.as_deref()?,
            self.listed_host.as_deref()?,
        ))
    }

    /// `CURRENT_USER()`: the account the login matched. Every account here
    /// is one for any host, `'name'@'%'`.
    pub(crate) fn current_user(&self) -> Option<String> {
        Some(format!("{}@%", self.account_name.as_deref()?))
    }

    /// The path of the Unix socket this server listens on, if it listens on
    /// one.
    ///
    /// A TCP server listens on none: the two listeners are never configured
    /// together.
    pub(crate) fn unix_socket_path(&self) -> Option<&[u8]> {
        self.unix_socket_path.as_deref()
    }
}
