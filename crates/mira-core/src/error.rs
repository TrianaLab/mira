use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Arrow(#[from] arrow_schema::ArrowError),

    /// A dictionary key space filled up mid-append. The caller's response is to
    /// seal the current block and retry into a fresh one, never to fail the RPC.
    #[error("dictionary key space exhausted for column `{0}`; seal the block")]
    DictionaryFull(&'static str),

    #[error("{path} is not an Arrow IPC file (bad ARROW1 magic)")]
    BadMagic { path: PathBuf },

    #[error("{path} failed checksum: footer says {expected:#010x}, body hashes to {actual:#010x}")]
    BadChecksum {
        path: PathBuf,
        expected: u32,
        actual: u32,
    },

    #[error("{path} has no {key} in its IPC footer metadata; not a Mira block")]
    MissingMetadata { path: PathBuf, key: &'static str },

    /// Raised when a block was written misaligned. We fail loudly rather than
    /// let arrow-rs silently memcpy the whole body out of the mapping, which
    /// would turn a zero-copy read into a full allocation without a signal.
    #[error("{path}: buffers are not aligned for zero-copy read: {source}")]
    Misaligned {
        path: PathBuf,
        #[source]
        source: arrow_schema::ArrowError,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) trait IoContext<T> {
    fn ctx(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn ctx(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }
}
