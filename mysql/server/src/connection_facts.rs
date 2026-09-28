//! What a connection knows about where it came from.

/// What a connection knows about itself, which `@@socket` answers.
///
/// A connection the runtime did not accept — one an embedding hands the
/// adapter directly — listened on no socket, which is what the empty value
/// says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MySqlConnectionFacts {
    unix_socket_path: Option<Vec<u8>>,
}

impl MySqlConnectionFacts {
    /// A connection accepted on the Unix socket at `path`.
    pub(crate) fn on_unix_socket(path: Vec<u8>) -> Self {
        assert!(!path.is_empty(), "a Unix socket has a path");
        Self {
            unix_socket_path: Some(path),
        }
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
