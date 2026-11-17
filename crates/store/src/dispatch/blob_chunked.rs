/*
 * SPDX-FileCopyrightText: 2026 Kaan88 and contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Chunked blobs.
//!
//! A large blob is stored as fixed-size chunks under random keys, each chunk
//! compressed and encrypted on its own, plus a small manifest stored raw under
//! the blob hash. Readers that ask for a range only fetch the chunks the range
//! covers, and writers never hold more than one chunk in memory.
//!
//! The manifest is recognised by its exact length, a magic prefix, a marker
//! byte that the blob codec never writes and a checksum, so an ordinary blob
//! cannot be taken for one. The main store marks a chunked blob by writing
//! [`CHUNKED_COMMIT_VALUE`] as the value of its `BlobOp::Commit` entry; the
//! purge uses that mark to delete the chunks together with the manifest.

use crate::{
    BlobStore,
    write::blob_codec::{BlobKeyring, BlobOpts},
};
use std::ops::Range;
use trc::AddContext;
use types::blob_hash::{BLOB_HASH_LEN, BlobHash};

/// Size of every chunk except the last one.
pub const CHUNK_SIZE: usize = 8 * 1024 * 1024;
/// Value of the `BlobOp::Commit` entry of a chunked blob (ordinary blobs have an empty value).
pub const CHUNKED_COMMIT_VALUE: u8 = 1;

pub const CHUNK_ID_LEN: usize = 16;
pub const CHUNK_KEY_LEN: usize = CHUNK_ID_LEN + 4;

const MANIFEST_MAGIC: &[u8; 8] = b"STWCHNK1";
const MANIFEST_MARKER: u8 = 0xc3;
const CHECKSUM_LEN: usize = 16;
const MANIFEST_BODY_LEN: usize = 8 + 8 + 4 + 4 + CHUNK_ID_LEN;
const MANIFEST_LEN: usize = MANIFEST_BODY_LEN + CHECKSUM_LEN + 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkManifest {
    pub size: u64,
    pub chunk_size: u32,
    pub chunk_count: u32,
    pub chunk_id: [u8; CHUNK_ID_LEN],
}

/// A chunked blob whose chunks are written but whose manifest may not be yet.
#[derive(Debug, Clone)]
pub struct ChunkedBlob {
    pub hash: BlobHash,
    pub manifest: ChunkManifest,
}

impl ChunkManifest {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MANIFEST_LEN);
        out.extend_from_slice(MANIFEST_MAGIC);
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.chunk_size.to_le_bytes());
        out.extend_from_slice(&self.chunk_count.to_le_bytes());
        out.extend_from_slice(&self.chunk_id);
        let checksum = blake3::hash(&out);
        out.extend_from_slice(&checksum.as_bytes()[..CHECKSUM_LEN]);
        out.push(MANIFEST_MARKER);
        out
    }

    /// Returns the manifest if `data` is one, `None` for any other blob.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() != MANIFEST_LEN
            || data.last() != Some(&MANIFEST_MARKER)
            || !data.starts_with(MANIFEST_MAGIC)
        {
            return None;
        }
        let (body, rest) = data.split_at(MANIFEST_BODY_LEN);
        if blake3::hash(body).as_bytes()[..CHECKSUM_LEN] != rest[..CHECKSUM_LEN] {
            return None;
        }
        let size = u64::from_le_bytes(body[8..16].try_into().ok()?);
        let chunk_size = u32::from_le_bytes(body[16..20].try_into().ok()?);
        let chunk_count = u32::from_le_bytes(body[20..24].try_into().ok()?);
        let chunk_id: [u8; CHUNK_ID_LEN] = body[24..24 + CHUNK_ID_LEN].try_into().ok()?;
        let manifest = ChunkManifest {
            size,
            chunk_size,
            chunk_count,
            chunk_id,
        };
        (chunk_size > 0 && u64::from(chunk_count) == size.div_ceil(u64::from(chunk_size)))
            .then_some(manifest)
    }

    pub fn chunk_key(&self, index: u32) -> [u8; CHUNK_KEY_LEN] {
        chunk_key(&self.chunk_id, index)
    }

    /// Length of chunk `index` once decoded.
    pub fn chunk_len(&self, index: u32) -> usize {
        let start = u64::from(index) * u64::from(self.chunk_size);
        (self.size.saturating_sub(start)).min(u64::from(self.chunk_size)) as usize
    }

    /// The chunks a byte range covers, each with the part of that chunk to keep.
    /// The range is clamped to the blob.
    pub fn chunks_for(&self, range: Range<u64>) -> impl Iterator<Item = (u32, Range<usize>)> + '_ {
        let end = range.end.min(self.size);
        let start = range.start.min(end);
        let chunk_size = u64::from(self.chunk_size);
        let first = (start / chunk_size) as u32;
        let last = if end == start {
            first
        } else {
            (end.div_ceil(chunk_size)) as u32
        };
        (first..last).map(move |index| {
            let chunk_start = u64::from(index) * chunk_size;
            let from = start.saturating_sub(chunk_start) as usize;
            let to = (end - chunk_start).min(chunk_size) as usize;
            (index, from..to)
        })
    }
}

fn chunk_key(chunk_id: &[u8; CHUNK_ID_LEN], index: u32) -> [u8; CHUNK_KEY_LEN] {
    let mut key = [0u8; CHUNK_KEY_LEN];
    key[..CHUNK_ID_LEN].copy_from_slice(chunk_id);
    key[CHUNK_ID_LEN..].copy_from_slice(&index.to_be_bytes());
    key
}

/// Writes a blob chunk by chunk while hashing it. Only one chunk is held in
/// memory. Call [`ChunkedBlobWriter::finish`] to get the hash and manifest, or
/// [`ChunkedBlobWriter::abort`] to delete what was written.
pub struct ChunkedBlobWriter {
    store: BlobStore,
    opts: BlobOpts,
    chunk_id: [u8; CHUNK_ID_LEN],
    buf: Vec<u8>,
    hasher: blake3::Hasher,
    size: u64,
    count: u32,
}

impl ChunkedBlobWriter {
    pub fn new(store: BlobStore, opts: BlobOpts) -> Self {
        let mut chunk_id = [0u8; CHUNK_ID_LEN];
        rand::fill(&mut chunk_id);
        ChunkedBlobWriter {
            store,
            opts,
            chunk_id,
            buf: Vec::new(),
            hasher: blake3::Hasher::new(),
            size: 0,
            count: 0,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub async fn write(&mut self, mut data: &[u8]) -> trc::Result<()> {
        self.hasher.update(data);
        self.size += data.len() as u64;
        while !data.is_empty() {
            if self.buf.capacity() == 0 {
                self.buf.reserve_exact(CHUNK_SIZE);
            }
            let take = (CHUNK_SIZE - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == CHUNK_SIZE {
                self.flush().await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> trc::Result<()> {
        let key = chunk_key(&self.chunk_id, self.count);
        let encoded = self.opts.encode(&key, &self.buf);
        self.store
            .write_blob(&key, encoded)
            .await
            .caused_by(trc::location!())?;
        self.count += 1;
        self.buf.clear();
        Ok(())
    }

    pub async fn finish(mut self) -> trc::Result<ChunkedBlob> {
        if !self.buf.is_empty()
            && let Err(err) = self.flush().await
        {
            self.abort().await;
            return Err(err);
        }
        Ok(ChunkedBlob {
            hash: BlobHash(*self.hasher.finalize().as_bytes()),
            manifest: ChunkManifest {
                size: self.size,
                chunk_size: CHUNK_SIZE as u32,
                chunk_count: self.count,
                chunk_id: self.chunk_id,
            },
        })
    }

    /// Deletes the chunks written so far. Failures are logged, not returned.
    pub async fn abort(self) {
        delete_chunks(&self.store, &self.chunk_id, self.count).await;
    }
}

async fn delete_chunks(store: &BlobStore, chunk_id: &[u8; CHUNK_ID_LEN], count: u32) {
    for index in 0..count {
        let key = chunk_key(chunk_id, index);
        if let Err(err) = store.delete_blob(&key).await {
            trc::error!(
                err.caused_by(trc::location!())
                    .details("Failed to delete blob chunk")
            );
        }
    }
}

impl ChunkedBlob {
    /// Stores the manifest under the blob hash, making the blob readable.
    pub async fn store_manifest(&self, store: &BlobStore) -> trc::Result<()> {
        store
            .write_blob(self.hash.as_slice(), Ok(self.manifest.serialize()))
            .await
            .caused_by(trc::location!())
    }

    /// Deletes the chunks of a blob whose manifest was never stored (for example
    /// because the same content is already in the store).
    pub async fn discard(&self, store: &BlobStore) {
        delete_chunks(store, &self.manifest.chunk_id, self.manifest.chunk_count).await;
    }
}

impl BlobStore {
    /// Reads and decodes one chunk.
    pub async fn get_chunk(
        &self,
        manifest: &ChunkManifest,
        index: u32,
        keys: &BlobKeyring,
    ) -> trc::Result<Vec<u8>> {
        let key = manifest.chunk_key(index);
        let raw = self
            .get_raw_blob(&key)
            .await
            .caused_by(trc::location!())?
            .ok_or_else(|| {
                trc::StoreEvent::DataCorruption
                    .into_err()
                    .details("Missing blob chunk")
                    .ctx(trc::Key::Key, key.as_slice())
                    .caused_by(trc::location!())
            })?;
        let data = keys.decode(&key, raw)?;
        if data.len() != manifest.chunk_len(index) {
            return Err(trc::StoreEvent::DataCorruption
                .into_err()
                .details("Blob chunk has the wrong length")
                .ctx(trc::Key::Key, key.as_slice())
                .caused_by(trc::location!()));
        }
        Ok(data)
    }

    /// Assembles a range of a chunked blob in memory.
    pub(crate) async fn get_chunked_range(
        &self,
        manifest: &ChunkManifest,
        range: Range<usize>,
        keys: &BlobKeyring,
    ) -> trc::Result<Vec<u8>> {
        let range = range.start as u64..u64::try_from(range.end).unwrap_or(u64::MAX);
        let mut out = Vec::new();
        for (index, part) in manifest.chunks_for(range) {
            let chunk = self.get_chunk(manifest, index, keys).await?;
            if out.capacity() == 0 {
                out.reserve_exact(
                    (manifest.size.min(usize::MAX as u64) as usize).min(64 * CHUNK_SIZE),
                );
            }
            out.extend_from_slice(&chunk[part]);
        }
        Ok(out)
    }

    /// Returns the manifest stored under `key`, if the blob there is chunked.
    /// This reads the whole blob when it is not chunked, so callers should
    /// first check the commit mark in the main store.
    pub async fn get_chunk_manifest(&self, key: &[u8]) -> trc::Result<Option<ChunkManifest>> {
        Ok(self
            .get_raw_blob(key)
            .await
            .caused_by(trc::location!())?
            .and_then(|raw| ChunkManifest::parse(&raw)))
    }

    /// Deletes the chunks of `manifest`, stopping at the first error. Deleting a
    /// chunk that is already gone is not an error.
    pub async fn delete_chunks(&self, manifest: &ChunkManifest) -> trc::Result<()> {
        for index in 0..manifest.chunk_count {
            self.delete_blob(&manifest.chunk_key(index))
                .await
                .caused_by(trc::location!())?;
        }
        Ok(())
    }

    /// Deletes a chunked blob: its chunks first, then the manifest. A blob that
    /// turns out not to be chunked is deleted as it is.
    pub async fn delete_chunked_blob(&self, key: &[u8]) -> trc::Result<()> {
        if key.len() == BLOB_HASH_LEN
            && let Some(manifest) = self.get_chunk_manifest(key).await?
        {
            delete_chunks(self, &manifest.chunk_id, manifest.chunk_count).await;
        }
        self.delete_blob(key).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(size: u64, chunk_size: u32) -> ChunkManifest {
        ChunkManifest {
            size,
            chunk_size,
            chunk_count: size.div_ceil(u64::from(chunk_size)) as u32,
            chunk_id: [7; CHUNK_ID_LEN],
        }
    }

    #[test]
    fn manifest_round_trip_and_rejects_others() {
        let m = manifest(20 * 1024 * 1024 + 3, CHUNK_SIZE as u32);
        let bytes = m.serialize();
        assert_eq!(bytes.len(), MANIFEST_LEN);
        assert_eq!(ChunkManifest::parse(&bytes), Some(m.clone()));

        // A flipped bit anywhere is not a manifest.
        for i in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[i] ^= 0x01;
            assert_eq!(ChunkManifest::parse(&bad), None, "byte {i}");
        }
        // Wrong lengths and ordinary blobs are not manifests.
        assert_eq!(ChunkManifest::parse(&bytes[1..]), None);
        assert_eq!(ChunkManifest::parse(b"hello world\0"), None);
        assert_eq!(ChunkManifest::parse(&[0u8; MANIFEST_LEN]), None);
        // A chunk count that does not match the size is refused.
        let mut wrong = m;
        wrong.chunk_count += 1;
        assert_eq!(ChunkManifest::parse(&wrong.serialize()), None);
    }

    #[test]
    fn chunks_for_ranges() {
        let m = manifest(25, 10);
        let all: Vec<_> = m.chunks_for(0..u64::MAX).collect();
        assert_eq!(all, vec![(0, 0..10), (1, 0..10), (2, 0..5)]);
        assert_eq!(m.chunks_for(3..7).collect::<Vec<_>>(), vec![(0, 3..7)]);
        assert_eq!(
            m.chunks_for(8..12).collect::<Vec<_>>(),
            vec![(0, 8..10), (1, 0..2)]
        );
        assert_eq!(m.chunks_for(10..20).collect::<Vec<_>>(), vec![(1, 0..10)]);
        assert_eq!(m.chunks_for(24..100).collect::<Vec<_>>(), vec![(2, 4..5)]);
        assert_eq!(m.chunks_for(25..30).count(), 0);
        assert_eq!(m.chunks_for(5..5).count(), 0);
        assert_eq!(m.chunk_len(0), 10);
        assert_eq!(m.chunk_len(2), 5);
        assert_eq!(m.chunk_len(3), 0);
    }

    #[test]
    fn chunk_keys_are_distinct_per_index() {
        let m = manifest(25, 10);
        assert_ne!(m.chunk_key(0), m.chunk_key(1));
        assert_eq!(&m.chunk_key(1)[..CHUNK_ID_LEN], &[7; CHUNK_ID_LEN]);
        assert_ne!(CHUNK_KEY_LEN, BLOB_HASH_LEN);
    }
}
