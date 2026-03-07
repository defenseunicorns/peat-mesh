//! Streaming large-blob transfer with resumable sessions (ADR-055)
//!
//! Provides a bounded-memory streaming pipeline for transferring arbitrarily
//! large blobs. Data flows from source to target in fixed-size chunks with
//! periodic checkpointing and incremental digest verification.
//!
//! # Design
//!
//! ```text
//! source.read_chunk() -> [SHA256 hasher] -> [checkpoint tracker] -> target.write_chunk()
//! ```
//!
//! Peak memory usage is O(chunk_size) regardless of blob size. On crash,
//! transfer resumes from the last checkpoint rather than restarting.
//!
//! # Chunk Size Tuning
//!
//! | Environment | Chunk Size | Checkpoint Interval | Rationale |
//! |-------------|-----------|-------------------|-----------  |
//! | Datacenter  | 32 MB     | Every 128 chunks  | Minimize syscall overhead |
//! | Tactical    | 8 MB      | Every 64 chunks   | Balance memory vs checkpoint frequency |
//! | Edge        | 1 MB      | Every 32 chunks   | Minimize re-transfer on failure |

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::debug;

/// Configuration for streaming blob transfer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamingTransferConfig {
    /// Size of each chunk in bytes (default: 8 MB).
    pub chunk_size: usize,
    /// Number of chunks between checkpoint saves (default: 64).
    pub checkpoint_interval: usize,
    /// Whether to verify the final digest (default: true).
    pub verify_digest: bool,
}

impl Default for StreamingTransferConfig {
    fn default() -> Self {
        Self::tactical()
    }
}

impl StreamingTransferConfig {
    /// Datacenter profile: high bandwidth, large chunks.
    pub fn datacenter() -> Self {
        Self {
            chunk_size: 32 * 1024 * 1024, // 32 MB
            checkpoint_interval: 128,
            verify_digest: true,
        }
    }

    /// Tactical profile: moderate bandwidth (default).
    pub fn tactical() -> Self {
        Self {
            chunk_size: 8 * 1024 * 1024, // 8 MB
            checkpoint_interval: 64,
            verify_digest: true,
        }
    }

    /// Edge profile: low bandwidth, small chunks.
    pub fn edge() -> Self {
        Self {
            chunk_size: 1024 * 1024, // 1 MB
            checkpoint_interval: 32,
            verify_digest: true,
        }
    }

    /// Custom profile.
    pub fn custom(chunk_size: usize, checkpoint_interval: usize) -> Self {
        Self {
            chunk_size,
            checkpoint_interval,
            verify_digest: true,
        }
    }

    /// Bytes between checkpoint saves.
    pub fn checkpoint_bytes(&self) -> u64 {
        self.chunk_size as u64 * self.checkpoint_interval as u64
    }
}

/// Checkpoint state for a streaming transfer in progress.
///
/// Serializable so it can be persisted to disk and recovered after crash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferCheckpoint {
    /// Unique transfer session identifier.
    pub session_id: String,
    /// Content digest being transferred.
    pub digest: String,
    /// Total blob size in bytes.
    pub total_size: u64,
    /// Bytes successfully transferred and checkpointed.
    pub offset: u64,
    /// Number of chunks completed.
    pub chunks_completed: u64,
    /// Serialized SHA256 hasher state (intermediate hash of bytes seen so far).
    /// We store the hash-so-far as bytes; on resume we cannot truly resume
    /// SHA256 mid-stream without re-reading, so this tracks verified offset.
    pub partial_sha256: Vec<u8>,
    /// Upload session URL (for OCI chunked upload resume).
    pub upload_session_url: Option<String>,
}

impl TransferCheckpoint {
    /// Create a new checkpoint for a fresh transfer.
    pub fn new(session_id: &str, digest: &str, total_size: u64) -> Self {
        Self {
            session_id: session_id.to_string(),
            digest: digest.to_string(),
            total_size,
            offset: 0,
            chunks_completed: 0,
            partial_sha256: Vec::new(),
            upload_session_url: None,
        }
    }

    /// Whether the transfer is complete.
    pub fn is_complete(&self) -> bool {
        self.offset >= self.total_size
    }

    /// Progress as a fraction (0.0 to 1.0).
    pub fn progress(&self) -> f64 {
        if self.total_size == 0 {
            return 1.0;
        }
        self.offset as f64 / self.total_size as f64
    }

    /// Bytes remaining.
    pub fn remaining(&self) -> u64 {
        self.total_size.saturating_sub(self.offset)
    }
}

/// Result of a streaming transfer operation.
#[derive(Clone, Debug)]
pub struct TransferResult {
    /// Total bytes transferred in this session (may be less than total if resumed).
    pub bytes_transferred: u64,
    /// Total blob size.
    pub total_size: u64,
    /// Computed SHA256 digest of the full content.
    pub computed_digest: String,
    /// Whether the transfer was resumed from a checkpoint.
    pub resumed: bool,
    /// Number of checkpoints saved during this transfer.
    pub checkpoints_saved: u64,
}

/// Callback for checkpoint persistence.
///
/// Called periodically during transfer so the caller can persist the
/// checkpoint to disk, database, or any durable store.
pub type CheckpointCallback = Box<dyn FnMut(&TransferCheckpoint) -> io::Result<()> + Send>;

/// Stream data from `source` to `target` in bounded-memory chunks.
///
/// Reads from `source`, computes incremental SHA256, and writes to `target`
/// in chunks of `config.chunk_size`. Periodically calls `on_checkpoint` to
/// allow the caller to persist progress.
///
/// # Arguments
///
/// * `source` - Async reader (e.g., HTTP response body, file)
/// * `target` - Async writer (e.g., HTTP upload body, file)
/// * `config` - Chunk size and checkpoint interval
/// * `checkpoint` - Mutable checkpoint state (pass a fresh one or a resumed one)
/// * `on_checkpoint` - Called every `checkpoint_interval` chunks with current state
///
/// # Returns
///
/// `TransferResult` with computed digest and transfer statistics.
pub async fn stream_transfer<R, W>(
    mut source: R,
    mut target: W,
    config: &StreamingTransferConfig,
    checkpoint: &mut TransferCheckpoint,
    mut on_checkpoint: Option<CheckpointCallback>,
) -> io::Result<TransferResult>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let resumed = checkpoint.offset > 0;
    let initial_offset = checkpoint.offset;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; config.chunk_size];
    let mut checkpoints_saved: u64 = 0;

    // If resuming, we need to skip `offset` bytes from source
    // (the caller should handle seeking if possible; we skip by reading and discarding)
    if resumed {
        let mut skip_remaining = checkpoint.offset;
        while skip_remaining > 0 {
            let to_read = (skip_remaining as usize).min(buf.len());
            let n = source.read(&mut buf[..to_read]).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "source ended at {} while skipping to offset {}",
                        checkpoint.total_size - skip_remaining,
                        checkpoint.offset
                    ),
                ));
            }
            // Feed skipped bytes into hasher to maintain correct digest
            hasher.update(&buf[..n]);
            skip_remaining -= n as u64;
        }
        debug!(
            session_id = %checkpoint.session_id,
            offset = checkpoint.offset,
            "resumed transfer, skipped to offset"
        );
    }

    // Main transfer loop
    loop {
        let n = source.read(&mut buf).await?;
        if n == 0 {
            break; // EOF
        }

        // Feed through hasher
        hasher.update(&buf[..n]);

        // Write to target
        target.write_all(&buf[..n]).await?;

        // Update checkpoint
        checkpoint.offset += n as u64;
        checkpoint.chunks_completed += 1;

        // Periodic checkpoint
        if checkpoint
            .chunks_completed
            .is_multiple_of(config.checkpoint_interval as u64)
        {
            if let Some(ref mut cb) = on_checkpoint {
                cb(checkpoint)?;
                checkpoints_saved += 1;
                debug!(
                    session_id = %checkpoint.session_id,
                    offset = checkpoint.offset,
                    progress = format!("{:.1}%", checkpoint.progress() * 100.0),
                    "checkpoint saved"
                );
            }
        }
    }

    target.flush().await?;

    // Compute final digest
    let hash = hasher.finalize();
    let computed_digest = format!("sha256:{}", hex::encode(hash));

    // Verify if requested
    if config.verify_digest && !checkpoint.digest.is_empty() && computed_digest != checkpoint.digest
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "digest mismatch: expected {}, computed {}",
                checkpoint.digest, computed_digest
            ),
        ));
    }

    let bytes_transferred = checkpoint.offset - initial_offset;

    Ok(TransferResult {
        bytes_transferred,
        total_size: checkpoint.offset,
        computed_digest,
        resumed,
        checkpoints_saved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_config_profiles() {
        let dc = StreamingTransferConfig::datacenter();
        assert_eq!(dc.chunk_size, 32 * 1024 * 1024);
        assert_eq!(dc.checkpoint_interval, 128);
        assert_eq!(dc.checkpoint_bytes(), 32 * 1024 * 1024 * 128);

        let tac = StreamingTransferConfig::tactical();
        assert_eq!(tac.chunk_size, 8 * 1024 * 1024);

        let edge = StreamingTransferConfig::edge();
        assert_eq!(edge.chunk_size, 1024 * 1024);
        assert_eq!(edge.checkpoint_interval, 32);
    }

    #[test]
    fn test_config_custom() {
        let c = StreamingTransferConfig::custom(4096, 10);
        assert_eq!(c.chunk_size, 4096);
        assert_eq!(c.checkpoint_interval, 10);
        assert_eq!(c.checkpoint_bytes(), 40960);
    }

    #[test]
    fn test_checkpoint_new() {
        let cp = TransferCheckpoint::new("sess-1", "sha256:abc", 1000);
        assert_eq!(cp.session_id, "sess-1");
        assert_eq!(cp.offset, 0);
        assert!(!cp.is_complete());
        assert_eq!(cp.remaining(), 1000);
        assert!((cp.progress() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_checkpoint_progress() {
        let mut cp = TransferCheckpoint::new("sess-1", "sha256:abc", 1000);
        cp.offset = 500;
        assert!((cp.progress() - 0.5).abs() < f64::EPSILON);
        assert_eq!(cp.remaining(), 500);
        assert!(!cp.is_complete());

        cp.offset = 1000;
        assert!(cp.is_complete());
        assert_eq!(cp.remaining(), 0);
    }

    #[test]
    fn test_checkpoint_zero_size() {
        let cp = TransferCheckpoint::new("sess-1", "", 0);
        assert!(cp.is_complete());
        assert!((cp.progress() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_checkpoint_serde_roundtrip() {
        let mut cp = TransferCheckpoint::new("sess-1", "sha256:abc", 5000);
        cp.offset = 2048;
        cp.chunks_completed = 4;
        cp.upload_session_url = Some("https://registry.example.com/upload/123".to_string());

        let json = serde_json::to_string(&cp).unwrap();
        let deserialized: TransferCheckpoint = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.session_id, "sess-1");
        assert_eq!(deserialized.offset, 2048);
        assert_eq!(deserialized.chunks_completed, 4);
        assert!(deserialized.upload_session_url.is_some());
    }

    #[tokio::test]
    async fn test_stream_transfer_small_blob() {
        let data = b"hello world, this is a test blob";
        let source = Cursor::new(data.to_vec());
        let mut target = Vec::new();

        let config = StreamingTransferConfig::custom(16, 2); // 16-byte chunks, checkpoint every 2
        let mut checkpoint = TransferCheckpoint::new("test-1", "", data.len() as u64);

        let result = stream_transfer(source, &mut target, &config, &mut checkpoint, None)
            .await
            .unwrap();

        assert_eq!(target, data);
        assert_eq!(result.bytes_transferred, data.len() as u64);
        assert_eq!(result.total_size, data.len() as u64);
        assert!(!result.resumed);
        assert!(result.computed_digest.starts_with("sha256:"));
    }

    #[tokio::test]
    async fn test_stream_transfer_with_checkpoints() {
        let data = vec![0xABu8; 1024]; // 1KB of 0xAB
        let source = Cursor::new(data.clone());
        let mut target = Vec::new();

        let config = StreamingTransferConfig::custom(100, 2); // 100-byte chunks, checkpoint every 2
        let mut checkpoint = TransferCheckpoint::new("test-2", "", data.len() as u64);

        let on_checkpoint: CheckpointCallback = Box::new(|_cp| Ok(()));

        let result = stream_transfer(
            source,
            &mut target,
            &config,
            &mut checkpoint,
            Some(on_checkpoint),
        )
        .await
        .unwrap();

        assert_eq!(target, data);
        assert!(result.checkpoints_saved > 0);
        // 1024 bytes / 100 byte chunks = ~10 chunks, checkpoint every 2 = ~5 checkpoints
        assert!(result.checkpoints_saved >= 4);
    }

    #[tokio::test]
    async fn test_stream_transfer_digest_verification() {
        let data = b"test data for digest verification";

        // Compute expected digest
        let mut hasher = Sha256::new();
        hasher.update(data);
        let expected = format!("sha256:{}", hex::encode(hasher.finalize()));

        // Transfer with correct expected digest
        let source = Cursor::new(data.to_vec());
        let mut target = Vec::new();
        let config = StreamingTransferConfig::custom(1024, 1);
        let mut checkpoint = TransferCheckpoint::new("test-3", &expected, data.len() as u64);

        let result = stream_transfer(source, &mut target, &config, &mut checkpoint, None)
            .await
            .unwrap();
        assert_eq!(result.computed_digest, expected);
    }

    #[tokio::test]
    async fn test_stream_transfer_digest_mismatch() {
        let data = b"test data";
        let source = Cursor::new(data.to_vec());
        let mut target = Vec::new();
        let config = StreamingTransferConfig::custom(1024, 1);
        let mut checkpoint = TransferCheckpoint::new("test-4", "sha256:wrong", data.len() as u64);

        let result = stream_transfer(source, &mut target, &config, &mut checkpoint, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("digest mismatch"));
    }

    #[tokio::test]
    async fn test_stream_transfer_resume() {
        // Simulate: first 50 bytes already transferred, resume from offset 50
        let data = vec![0xCDu8; 200];
        let source = Cursor::new(data.clone());
        let mut target = Vec::new();

        let config = StreamingTransferConfig::custom(50, 1);
        let mut checkpoint = TransferCheckpoint::new("test-5", "", data.len() as u64);
        checkpoint.offset = 50; // Already transferred 50 bytes

        let result = stream_transfer(source, &mut target, &config, &mut checkpoint, None)
            .await
            .unwrap();

        assert!(result.resumed);
        // Only the remaining 150 bytes should be written to target
        assert_eq!(result.bytes_transferred, 150);
        assert_eq!(target.len(), 150);
        assert_eq!(result.total_size, 200);
    }

    #[tokio::test]
    async fn test_stream_transfer_empty() {
        let source = Cursor::new(Vec::<u8>::new());
        let mut target = Vec::new();
        let config = StreamingTransferConfig::custom(1024, 1);
        let mut checkpoint = TransferCheckpoint::new("test-6", "", 0);

        let result = stream_transfer(source, &mut target, &config, &mut checkpoint, None)
            .await
            .unwrap();

        assert!(target.is_empty());
        assert_eq!(result.bytes_transferred, 0);
        assert!(checkpoint.is_complete());
    }

    #[tokio::test]
    async fn test_stream_transfer_exact_chunk_boundary() {
        // Blob size exactly divisible by chunk size
        let data = vec![0xEFu8; 300];
        let source = Cursor::new(data.clone());
        let mut target = Vec::new();
        let config = StreamingTransferConfig::custom(100, 1);
        let mut checkpoint = TransferCheckpoint::new("test-7", "", 300);

        let result = stream_transfer(source, &mut target, &config, &mut checkpoint, None)
            .await
            .unwrap();

        assert_eq!(target, data);
        assert_eq!(result.bytes_transferred, 300);
    }

    #[tokio::test]
    async fn test_stream_transfer_checkpoint_callback_error() {
        let data = vec![0u8; 500];
        let source = Cursor::new(data);
        let mut target = Vec::new();
        let config = StreamingTransferConfig::custom(50, 2);
        let mut checkpoint = TransferCheckpoint::new("test-8", "", 500);

        let on_checkpoint: CheckpointCallback = Box::new(|_cp| {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "checkpoint store full",
            ))
        });

        let result = stream_transfer(
            source,
            &mut target,
            &config,
            &mut checkpoint,
            Some(on_checkpoint),
        )
        .await;

        assert!(result.is_err());
    }

    #[test]
    fn test_transfer_result_fields() {
        let result = TransferResult {
            bytes_transferred: 5000,
            total_size: 10000,
            computed_digest: "sha256:abc".to_string(),
            resumed: true,
            checkpoints_saved: 3,
        };
        assert_eq!(result.bytes_transferred, 5000);
        assert!(result.resumed);
    }
}
