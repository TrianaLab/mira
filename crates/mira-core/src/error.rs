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

    /// Every way arrow-rs can refuse a block body: a misaligned buffer, a
    /// dictionary that will not decode, a ZSTD frame that will not decompress.
    /// One variant because they arrive through one call and the `source` is
    /// what says which — and named for the refusal rather than for alignment,
    /// because a headline of "buffers are not aligned" over a corrupt ZSTD
    /// frame sends the reader to the wrong half of the file at 3am.
    ///
    /// Misalignment is the case worth naming in the message anyway: it is the
    /// one Mira opts into detecting, with `with_require_alignment(true)`. The
    /// arrow-rs default is to silently memcpy the whole body out of the
    /// mapping, turning a zero-copy read into a full allocation with no signal.
    #[error("{path}: this block's body cannot be decoded: {source}")]
    Undecodable {
        path: PathBuf,
        #[source]
        source: arrow_schema::ArrowError,
    },

    /// The data directory is on a filesystem Mira's read path cannot survive.
    /// See `block::check_filesystem`.
    #[error(
        "{path} is on {fs}, a network filesystem. Mira reads blocks through mmap, \
         and on {fs} a server-side error surfaces as SIGBUS — a signal, not an \
         error, with no recovery path from Rust. Point --data-dir at a local \
         block device (in Kubernetes: a local PV, an EBS/PD volume, or an \
         emptyDir, not an NFS/CSI network mount)."
    )]
    NetworkFilesystem { path: PathBuf, fs: String },

    /// The data directory exists but cannot be written to. See
    /// `block::check_writable`.
    #[error(
        "{path} is not writable: {source}. Mira writes nowhere else, so this is \
         fatal at startup rather than degraded at 3am. Check that the mount is \
         read-write and that this process owns the path (in Kubernetes: a \
         volume mounted readOnly, or a missing fsGroup)."
    )]
    NotWritable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// An export bigger than the WAL will frame. Distinct from a corrupt
    /// length on read: this one is the caller's fault and is answerable with a
    /// 4xx, so it must not be confused with the file being damaged.
    #[error("export of {len} bytes exceeds the {max}-byte WAL frame limit")]
    WalFrameTooLarge { len: usize, max: u32 },

    /// A WAL frame did not survive the trip. Expected exactly once, at the
    /// tail of the last segment after a crash; anywhere else it is damage.
    /// See `wal::Wal::replay` for why this ends a segment rather than the
    /// process.
    #[error("{path}: corrupt write-ahead log frame: {why}")]
    WalCorrupt { path: PathBuf, why: &'static str },

    /// A WAL segment written by a different build. Refused rather than
    /// guessed at, for the same reason a block with an unknown format version
    /// is: a frame layout is not self-describing enough to parse hopefully.
    #[error("{path}: write-ahead log version {found}, this build writes {expected}")]
    WalVersion {
        path: PathBuf,
        found: u16,
        expected: u16,
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
