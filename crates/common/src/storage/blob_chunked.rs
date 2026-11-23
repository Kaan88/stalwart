/*
 * SPDX-FileCopyrightText: 2026 Kaan88 and contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Server-level helpers for chunked blobs (see `store::dispatch::blob_chunked`).

use crate::{Server, storage::blob::FetchedBlob};
use std::ops::Range;
use store::{
    ValueKey,
    dispatch::blob_chunked::{CHUNKED_COMMIT_VALUE, ChunkManifest, ChunkedBlob},
    write::{
        BatchBuilder, BlobLink, BlobOp, ValueClass, blob::BlobLinkValue, now, serialize::RawValue,
    },
};
use trc::AddContext;
use types::{
    blob::{BlobClass, BlobId},
    blob_hash::BlobHash,
};

/// A blob as it can be served over HTTP: the whole content in memory when it
/// is small, or its manifest when it is chunked and must be streamed.
pub enum ServableBlob {
    Memory(FetchedBlob),
    Chunked(ChunkManifest),
}

impl Server {
    /// Whether the blob was committed as a chunked blob.
    pub async fn blob_is_chunked(&self, hash: &BlobHash) -> trc::Result<bool> {
        self.store()
            .get_value::<RawValue>(ValueKey {
                account_id: 0,
                collection: 0,
                document_id: 0,
                class: ValueClass::Blob(BlobOp::Commit { hash: hash.clone() }),
            })
            .await
            .caused_by(trc::location!())
            .map(|value| value.is_some_and(|value| value.0 == [CHUNKED_COMMIT_VALUE]))
    }

    /// The manifest of a chunked blob, `None` for an ordinary or missing blob.
    /// Cheap for ordinary blobs: only the commit entry is read.
    pub async fn chunk_manifest(&self, hash: &BlobHash) -> trc::Result<Option<ChunkManifest>> {
        if self.blob_is_chunked(hash).await? {
            self.blob_store()
                .get_chunk_manifest(hash.as_slice())
                .await
                .caused_by(trc::location!())
        } else {
            Ok(None)
        }
    }

    /// Fetches a blob for serving. Chunked blobs are returned as their manifest
    /// so the caller can stream them; anything else is read into memory.
    pub async fn servable_blob(
        &self,
        hash: &BlobHash,
        size_hint: Option<u64>,
    ) -> trc::Result<Option<ServableBlob>> {
        if let Some(manifest) = self.chunk_manifest(hash).await? {
            return Ok(Some(ServableBlob::Chunked(manifest)));
        }
        Ok(self
            .fetch_blob(hash, 0..usize::MAX, size_hint)
            .await?
            .map(ServableBlob::Memory))
    }

    /// Reads one decoded chunk.
    pub async fn blob_chunk(&self, manifest: &ChunkManifest, index: u32) -> trc::Result<Vec<u8>> {
        self.blob_store()
            .get_chunk(manifest, index, &self.core.blob.keys)
            .await
    }

    /// Makes a chunked upload a stored blob held for `account_id` until `hold_for`
    /// seconds from now, like `put_jmap_blob` does for a blob held in memory.
    /// When the same content is already stored, the new chunks are deleted.
    pub async fn commit_chunked_blob(
        &self,
        account_id: u32,
        blob: ChunkedBlob,
        hold_for: u64,
    ) -> trc::Result<BlobId> {
        let until = now() + hold_for;
        let mut batch = BatchBuilder::new();
        batch.with_account_id(account_id).set(
            BlobOp::Link {
                hash: blob.hash.clone(),
                to: BlobLink::Temporary { until },
            },
            BlobLinkValue::new(blob.manifest.size),
        );
        self.store()
            .write_batch(&mut batch)
            .await
            .caused_by(trc::location!())?;

        if self
            .store()
            .blob_exists(&blob.hash)
            .await
            .caused_by(trc::location!())?
        {
            blob.discard(self.blob_store()).await;
        } else {
            if let Err(err) = blob.store_manifest(self.blob_store()).await {
                blob.discard(self.blob_store()).await;
                return Err(err);
            }
            let mut batch = BatchBuilder::new();
            batch.set(
                BlobOp::Commit {
                    hash: blob.hash.clone(),
                },
                vec![CHUNKED_COMMIT_VALUE],
            );
            self.store()
                .write_batch(&mut batch)
                .await
                .caused_by(trc::location!())?;
        }

        Ok(BlobId {
            hash: blob.hash,
            class: BlobClass::Reserved {
                account_id,
                expires: until,
            },
            section: None,
        })
    }
}

/// Parses a single-range `Range` header value (`bytes=a-b`, `bytes=a-`,
/// `bytes=-n`) against a blob of `size` bytes. `Ok(None)` means no usable
/// range (serve the whole blob), `Err(())` an unsatisfiable one (416).
#[allow(clippy::result_unit_err)]
pub fn parse_byte_range(value: &str, size: u64) -> Result<Option<Range<u64>>, ()> {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        // Multiple ranges are not supported; serve the whole blob.
        return Ok(None);
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let (start, end) = (start.trim(), end.trim());
    let range = if start.is_empty() {
        let Ok(suffix) = end.parse::<u64>() else {
            return Ok(None);
        };
        if suffix == 0 {
            return Err(());
        }
        size.saturating_sub(suffix)..size
    } else {
        let Ok(start) = start.parse::<u64>() else {
            return Ok(None);
        };
        let end = if end.is_empty() {
            size
        } else {
            match end.parse::<u64>() {
                Ok(end) => end.saturating_add(1).min(size),
                Err(_) => return Ok(None),
            }
        };
        if start >= size || start >= end {
            return Err(());
        }
        start..end
    };
    if range.start >= range.end {
        Err(())
    } else {
        Ok(Some(range))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_byte_range;

    #[test]
    fn byte_ranges() {
        assert_eq!(parse_byte_range("bytes=0-9", 100), Ok(Some(0..10)));
        assert_eq!(parse_byte_range("bytes=90-", 100), Ok(Some(90..100)));
        assert_eq!(parse_byte_range("bytes=-10", 100), Ok(Some(90..100)));
        assert_eq!(parse_byte_range("bytes=-500", 100), Ok(Some(0..100)));
        assert_eq!(parse_byte_range("bytes=50-500", 100), Ok(Some(50..100)));
        assert_eq!(parse_byte_range("bytes=100-", 100), Err(()));
        assert_eq!(parse_byte_range("bytes=5-1", 100), Err(()));
        assert_eq!(parse_byte_range("bytes=-0", 100), Err(()));
        assert_eq!(parse_byte_range("bytes=0-1,5-6", 100), Ok(None));
        assert_eq!(parse_byte_range("items=0-1", 100), Ok(None));
        assert_eq!(parse_byte_range("bytes=x-1", 100), Ok(None));
        assert_eq!(parse_byte_range("bytes=0-0", 0), Err(()));
    }
}
