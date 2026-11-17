/*
 * SPDX-FileCopyrightText: 2026 Kaan88 and contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Offline tools that take a data store back to what upstream Stalwart reads.
//!
//! Run with the server stopped: `STALWART_FORK_TOOL=<tool> stalwart --config=...`.
//! The binary loads its configuration, runs the tool and exits; no listener is
//! started and no background task runs.
//!
//! * `report`: lists every chunked blob with what refers to it, and counts the
//!   fork-only records (chunked marks, manifests, chunk keys, share links).
//! * `convert`: rewrites every chunked blob as an ordinary blob (checked against
//!   its hash), deletes its chunks and mark, and deletes all share links. Files,
//!   mail and everything else stay. A blob larger than the memory limit
//!   (`STALWART_FORK_MAX_MEMORY`, default 1 GiB) is refused and reported.
//! * `nuke`: deletes the files whose content is a chunked blob, as a delete by
//!   the owner would (quota, change log), deletes chunked blobs nothing else
//!   uses, converts those that are still used (mail, for example) and deletes
//!   all share links.
//!
//! Both end with the `report` counts, which must be zero, and both can be run
//! again: a second run finds nothing to do. `convert` is safe to interrupt: before
//! a blob is rewritten its manifest is kept in its commit record
//! ([`JOURNAL_TAG`]), so a later run finishes it. `STALWART_FORK_CRASH_AT`
//! (tests only) aborts at either step of the first blob.

use common::{Server, storage::metadata::ContainerCleanup};
use groupware::{
    DestroyArchive,
    cache::GroupwareCache,
    file::{FileNode, share_link::SHARE_LINKS_FIELD},
};
use std::collections::BTreeMap;
use store::{
    IterateParams, U32_LEN, U64_LEN, ValueKey,
    dispatch::blob_chunked::ChunkManifest,
    write::{
        Archive, ArchiveBytes, BatchBuilder, BlobLink, BlobOp, ValueClass,
        key::DeserializeBigEndian,
    },
};
use trc::AddContext;
use types::{
    blob_hash::{BLOB_HASH_LEN, BlobHash},
    collection::{Collection, SyncCollection},
};

// Output never panics: a full disk (where logs and redirected output often
// live) must end the tool with an error, not with a failed print.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}
macro_rules! say_err {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

/// Commit value of a chunked blob that `convert` is rewriting: this tag, then
/// the manifest.
pub const JOURNAL_TAG: u8 = 2;
const DEFAULT_MAX_MEMORY: u64 = 1024 * 1024 * 1024;
const FILE_COLLECTION: u8 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Referrer {
    Document {
        account_id: u32,
        collection: u8,
        document_id: u32,
    },
    Temporary {
        account_id: u32,
        until: u64,
    },
    Id {
        id: u64,
    },
}

#[derive(Debug, Default)]
struct BlobRecord {
    commit_value: Option<Vec<u8>>,
    referrers: Vec<Referrer>,
}

impl BlobRecord {
    fn is_fork_marked(&self) -> bool {
        self.commit_value
            .as_ref()
            .is_some_and(|value| !value.is_empty())
    }

    fn journal(&self) -> Option<ChunkManifest> {
        self.commit_value
            .as_ref()
            .filter(|value| value.first() == Some(&JOURNAL_TAG))
            .and_then(|value| ChunkManifest::parse(&value[1..]))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub chunked_marks: u64,
    pub manifests: u64,
    pub chunk_keys: u64,
    pub share_link_records: u64,
}

impl Counts {
    fn is_zero(&self) -> bool {
        *self == Counts::default()
    }

    fn print(&self, label: &str) {
        say!(
            "{label}: chunked_marks={} manifests={} chunk_keys={} share_link_records={}",
            self.chunked_marks,
            self.manifests,
            self.chunk_keys,
            self.share_link_records
        );
    }
}

/// Runs the tool named in `STALWART_FORK_TOOL`. Returns whether it succeeded.
pub async fn run_fork_tool(server: &Server, tool: &str) -> bool {
    let max_memory = std::env::var("STALWART_FORK_MAX_MEMORY")
        .ok()
        .and_then(|value| parse_size(&value))
        .unwrap_or(DEFAULT_MAX_MEMORY);
    let result = match tool {
        "report" => report(server, true).await.map(|_| true),
        "convert" => convert(server, max_memory).await,
        "nuke" => nuke(server, max_memory).await,
        _ => {
            say_err!("unknown STALWART_FORK_TOOL {tool:?}: use report, convert or nuke");
            return false;
        }
    };
    match result {
        Ok(ok) => ok,
        Err(err) => {
            say_err!("{tool} failed: {err}");
            false
        }
    }
}

fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim();
    let (number, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => value.split_at(at),
        None => (value, ""),
    };
    let number: u64 = number.parse().ok()?;
    let factor = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        _ => return None,
    };
    number.checked_mul(factor)
}

fn short(hash: &BlobHash) -> String {
    hash.as_slice()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Every blob with a commit record, keyed by hash, with what refers to it.
async fn scan_blobs(server: &Server) -> trc::Result<BTreeMap<BlobHash, BlobRecord>> {
    let from_key = ValueKey {
        account_id: 0,
        collection: 0,
        document_id: 0,
        class: ValueClass::Blob(BlobOp::Commit {
            hash: BlobHash::default(),
        }),
    };
    let to_key = ValueKey {
        account_id: u32::MAX,
        collection: u8::MAX,
        document_id: u32::MAX,
        class: ValueClass::Blob(BlobOp::Link {
            hash: BlobHash::new_max(),
            to: BlobLink::Document,
        }),
    };
    let mut blobs: BTreeMap<BlobHash, BlobRecord> = BTreeMap::new();
    server
        .store()
        .iterate(
            IterateParams::new(from_key, to_key).ascending(),
            |key, value| {
                let Some(hash) = key
                    .get(..BLOB_HASH_LEN)
                    .and_then(|slice| BlobHash::try_from_hash_slice(slice).ok())
                else {
                    return Ok(true);
                };
                let record = blobs.entry(hash).or_default();
                let rest = key.len() - BLOB_HASH_LEN;
                if rest == 0 {
                    record.commit_value = Some(value.to_vec());
                } else if rest == U32_LEN * 2 + 1 {
                    record.referrers.push(Referrer::Document {
                        account_id: key.deserialize_be_u32(BLOB_HASH_LEN)?,
                        collection: key[BLOB_HASH_LEN + U32_LEN],
                        document_id: key.deserialize_be_u32(BLOB_HASH_LEN + U32_LEN + 1)?,
                    });
                } else if rest == U32_LEN + U64_LEN {
                    record.referrers.push(Referrer::Temporary {
                        account_id: key.deserialize_be_u32(BLOB_HASH_LEN)?,
                        until: key.deserialize_be_u64(BLOB_HASH_LEN + U32_LEN)?,
                    });
                } else if rest == U64_LEN {
                    record.referrers.push(Referrer::Id {
                        id: key.deserialize_be_u64(BLOB_HASH_LEN)?,
                    });
                }
                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;
    Ok(blobs)
}

/// (account, document) of every FileNode that has share links.
async fn scan_share_links(server: &Server) -> trc::Result<Vec<(u32, u32)>> {
    let from_key = ValueKey {
        account_id: 0,
        collection: FILE_COLLECTION,
        document_id: 0,
        class: ValueClass::Property(SHARE_LINKS_FIELD),
    };
    let to_key = ValueKey {
        account_id: u32::MAX,
        collection: FILE_COLLECTION,
        document_id: u32::MAX,
        class: ValueClass::Property(SHARE_LINKS_FIELD),
    };
    let mut found = Vec::new();
    server
        .store()
        .iterate(
            IterateParams::new(from_key, to_key)
                .ascending()
                .set_values(false),
            |key, _| {
                // account (4) | collection (1) | field (1) | document (4)
                if key.len() == U32_LEN * 2 + 2
                    && key[U32_LEN] == FILE_COLLECTION
                    && key[U32_LEN + 1] == SHARE_LINKS_FIELD
                {
                    found.push((
                        key.deserialize_be_u32(0)?,
                        key.deserialize_be_u32(U32_LEN + 2)?,
                    ));
                }
                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;
    Ok(found)
}

/// The manifest stored under `hash`, if the blob there is still chunked.
/// Reads the whole blob when it is not, so only call it for marked blobs.
async fn stored_manifest(server: &Server, hash: &BlobHash) -> trc::Result<Option<ChunkManifest>> {
    server
        .blob_store()
        .get_chunk_manifest(hash.as_slice())
        .await
        .caused_by(trc::location!())
}

async fn report(server: &Server, verbose: bool) -> trc::Result<Counts> {
    let blobs = scan_blobs(server).await?;
    let mut counts = Counts::default();
    for (hash, record) in &blobs {
        if !record.is_fork_marked() {
            continue;
        }
        counts.chunked_marks += 1;
        let manifest = stored_manifest(server, hash).await?;
        if let Some(manifest) = &manifest {
            counts.manifests += 1;
            counts.chunk_keys += u64::from(manifest.chunk_count);
        } else if let Some(journal) = record.journal() {
            // Rewritten whole by an interrupted convert; its chunks may still exist.
            counts.chunk_keys += u64::from(journal.chunk_count);
        }
        if verbose {
            let size = manifest
                .as_ref()
                .or(record.journal().as_ref())
                .map_or_else(|| "?".to_string(), |m| m.size.to_string());
            say!(
                "chunked blob {} size={} state={} referrers={}",
                short(hash),
                size,
                match (&manifest, record.journal()) {
                    (Some(_), None) => "chunked",
                    (Some(_), Some(_)) => "converting (not yet rewritten)",
                    (None, Some(_)) => "converting (rewritten, chunks pending)",
                    (None, None) => "marked without manifest",
                },
                describe_referrers(&record.referrers)
            );
        }
    }
    let links = scan_share_links(server).await?;
    counts.share_link_records = links.len() as u64;
    if verbose {
        for (account_id, document_id) in &links {
            say!("share links on file account={account_id} document={document_id}");
        }
    }
    counts.print("report");
    Ok(counts)
}

fn describe_referrers(referrers: &[Referrer]) -> String {
    if referrers.is_empty() {
        return "none".to_string();
    }
    referrers
        .iter()
        .map(|referrer| match referrer {
            Referrer::Document {
                account_id,
                collection,
                document_id,
            } => match Collection::from(*collection) {
                Collection::FileNode => format!("file({account_id}/{document_id})"),
                Collection::Email => format!("email({account_id}/{document_id})"),
                other => format!("{}({account_id}/{document_id})", other.as_str()),
            },
            Referrer::Temporary { account_id, .. } => format!("upload({account_id})"),
            Referrer::Id { id } => format!("queue({id})"),
        })
        .collect::<Vec<_>>()
        .join(",")
}

enum ConvertOutcome {
    Converted(u64),
    Refused(u64),
}

/// Rewrites one chunked blob as an ordinary blob. Safe to interrupt and rerun.
async fn convert_blob(
    server: &Server,
    hash: &BlobHash,
    record: &BlobRecord,
    max_memory: u64,
) -> trc::Result<ConvertOutcome> {
    let blob_store = server.blob_store();
    let journal = record.journal();
    let stored = stored_manifest(server, hash).await?;

    if let Some(manifest) = &stored {
        if manifest.size > max_memory {
            return Ok(ConvertOutcome::Refused(manifest.size));
        }
        // 1. Keep the manifest in the commit record, so an interrupted run can finish.
        if journal.as_ref() != Some(manifest) {
            let mut value = Vec::with_capacity(64);
            value.push(JOURNAL_TAG);
            value.extend_from_slice(&manifest.serialize());
            write_commit_value(server, hash, value).await?;
        }
        crash_point("journal");
        // 2. Read the whole content and check it against the hash.
        let data = blob_store
            .get_blob(hash.as_slice(), 0..usize::MAX, &server.core.blob.keys)
            .await
            .caused_by(trc::location!())?
            .ok_or_else(|| missing(hash))?;
        if data.len() as u64 != manifest.size || BlobHash::generate(&data) != *hash {
            return Err(trc::StoreEvent::DataCorruption
                .into_err()
                .details("Chunked blob does not match its hash; left as it is")
                .ctx(trc::Key::Key, hash.as_slice().to_vec()));
        }
        // 3. Store it whole under the same key, replacing the manifest.
        blob_store
            .put_blob(hash.as_slice(), &data, &server.core.blob.file)
            .await
            .caused_by(trc::location!())?;
        drop(data);
        crash_point("rewrite");
        // 4. Delete the chunks, then the mark.
        blob_store.delete_chunks(manifest).await?;
        write_commit_value(server, hash, Vec::new()).await?;
        Ok(ConvertOutcome::Converted(manifest.size))
    } else if let Some(journal) = journal {
        // Rewritten whole by an earlier run that stopped before deleting the chunks.
        let data = blob_store
            .get_blob(hash.as_slice(), 0..usize::MAX, &server.core.blob.keys)
            .await
            .caused_by(trc::location!())?
            .ok_or_else(|| missing(hash))?;
        if BlobHash::generate(&data) != *hash {
            return Err(trc::StoreEvent::DataCorruption
                .into_err()
                .details("Rewritten blob does not match its hash; chunks kept")
                .ctx(trc::Key::Key, hash.as_slice().to_vec()));
        }
        drop(data);
        blob_store.delete_chunks(&journal).await?;
        write_commit_value(server, hash, Vec::new()).await?;
        Ok(ConvertOutcome::Converted(journal.size))
    } else {
        // Marked, but there is no manifest and no journal: nothing to rewrite.
        write_commit_value(server, hash, Vec::new()).await?;
        Ok(ConvertOutcome::Converted(0))
    }
}

/// For the compatibility tests only: `STALWART_FORK_CRASH_AT=journal|rewrite`
/// aborts the process at that point of the first blob it converts, as a crash
/// or `kill -9` would, so the recovery of an interrupted convert can be tested
/// deterministically.
fn crash_point(point: &str) {
    if std::env::var("STALWART_FORK_CRASH_AT").is_ok_and(|value| value == point) {
        say_err!("STALWART_FORK_CRASH_AT={point}: aborting");
        std::process::abort();
    }
}

fn missing(hash: &BlobHash) -> trc::Error {
    trc::StoreEvent::DataCorruption
        .into_err()
        .details("Chunked blob is missing")
        .ctx(trc::Key::Key, hash.as_slice().to_vec())
}

async fn write_commit_value(server: &Server, hash: &BlobHash, value: Vec<u8>) -> trc::Result<()> {
    let mut batch = BatchBuilder::new();
    batch.set(BlobOp::Commit { hash: hash.clone() }, value);
    server
        .store()
        .write_batch(&mut batch)
        .await
        .caused_by(trc::location!())
        .map(|_| ())
}

async fn delete_share_links(server: &Server) -> trc::Result<u64> {
    let links = scan_share_links(server).await?;
    for (account_id, document_id) in &links {
        let mut batch = BatchBuilder::new();
        batch
            .with_account_id(*account_id)
            .with_collection(Collection::FileNode)
            .with_document(*document_id)
            .clear(ValueClass::Property(SHARE_LINKS_FIELD));
        server
            .store()
            .write_batch(&mut batch)
            .await
            .caused_by(trc::location!())?;
    }
    Ok(links.len() as u64)
}

async fn convert(server: &Server, max_memory: u64) -> trc::Result<bool> {
    let blobs = scan_blobs(server).await?;
    let mut refused = 0;
    for (hash, record) in &blobs {
        if !record.is_fork_marked() {
            continue;
        }
        match convert_blob(server, hash, record, max_memory).await? {
            ConvertOutcome::Converted(size) => {
                say!("converted blob {} size={size}", short(hash))
            }
            ConvertOutcome::Refused(size) => {
                refused += 1;
                say!(
                    "refused blob {} size={size}: larger than the memory limit of {max_memory} bytes \
                     (set STALWART_FORK_MAX_MEMORY); used by {}",
                    short(hash),
                    describe_referrers(&record.referrers)
                );
            }
        }
    }
    let deleted = delete_share_links(server).await?;
    say!("deleted share links on {deleted} files");
    finish(server, refused).await
}

async fn nuke(server: &Server, max_memory: u64) -> trc::Result<bool> {
    let blobs = scan_blobs(server).await?;
    let mut refused = 0;
    for (hash, record) in &blobs {
        if !record.is_fork_marked() {
            continue;
        }
        // 1. Delete the files whose content this is.
        for referrer in &record.referrers {
            if let Referrer::Document {
                account_id,
                collection: FILE_COLLECTION,
                document_id,
            } = *referrer
            {
                delete_file(server, hash, account_id, document_id).await?;
            }
        }
        // 2. Whatever else still uses the blob keeps it, converted.
        let still_used = record.referrers.iter().any(|referrer| match referrer {
            Referrer::Document { collection, .. } => *collection != FILE_COLLECTION,
            Referrer::Id { .. } => true,
            Referrer::Temporary { .. } => false,
        });
        if still_used {
            match convert_blob(server, hash, record, max_memory).await? {
                ConvertOutcome::Converted(size) => say!(
                    "converted blob {} size={size}, still used by {}",
                    short(hash),
                    describe_referrers(&record.referrers)
                ),
                ConvertOutcome::Refused(size) => {
                    refused += 1;
                    say!(
                        "refused blob {} size={size}: larger than the memory limit, still used by {}",
                        short(hash),
                        describe_referrers(&record.referrers)
                    );
                }
            }
        } else {
            // 3. Nothing else uses it: delete chunks, manifest, uploads and the record.
            delete_blob_entirely(server, hash, record).await?;
            say!("deleted blob {}", short(hash));
        }
    }
    let deleted = delete_share_links(server).await?;
    say!("deleted share links on {deleted} files");
    finish(server, refused).await
}

async fn delete_file(
    server: &Server,
    hash: &BlobHash,
    account_id: u32,
    document_id: u32,
) -> trc::Result<()> {
    let Some(archive) = server
        .store()
        .get_value::<Archive<ArchiveBytes>>(ValueKey::archive(
            account_id,
            Collection::FileNode,
            document_id,
        ))
        .await
        .caused_by(trc::location!())?
    else {
        return Ok(());
    };
    let node = archive
        .to_unarchived::<FileNode>()
        .caused_by(trc::location!())?;
    let Some(file) = node.inner.file() else {
        return Ok(());
    };
    if file.blob_hash.0.as_slice() != hash.as_slice() {
        return Ok(());
    }
    let name = node.inner.name.to_string();
    let size = u32::from(file.size);
    let resources = server
        .fetch_groupware_resources(account_id, account_id, SyncCollection::FileNode)
        .await
        .caused_by(trc::location!())?;
    let path = resources
        .any_resource_path_by_id(document_id)
        .map(|path| resources.format_resource(path))
        .unwrap_or_default();
    let changed_by = server.account(account_id).await?.changed_by();
    let mut batch = BatchBuilder::new();
    DestroyArchive(node)
        .delete_with_cleanup(
            server,
            changed_by,
            account_id,
            document_id,
            &mut batch,
            path,
            &ContainerCleanup::empty(Collection::FileNode),
        )
        .await?;
    server
        .commit_batch(batch)
        .await
        .caused_by(trc::location!())?;
    say!("deleted file account={account_id} document={document_id} size={size} name={name:?}");
    Ok(())
}

async fn delete_blob_entirely(
    server: &Server,
    hash: &BlobHash,
    record: &BlobRecord,
) -> trc::Result<()> {
    let blob_store = server.blob_store();
    if let Some(manifest) = stored_manifest(server, hash).await? {
        blob_store.delete_chunks(&manifest).await?;
    }
    if let Some(journal) = record.journal() {
        blob_store.delete_chunks(&journal).await?;
    }
    blob_store
        .delete_blob(hash.as_slice())
        .await
        .caused_by(trc::location!())?;
    let mut batch = BatchBuilder::new();
    for referrer in &record.referrers {
        if let Referrer::Temporary { account_id, until } = *referrer {
            batch.with_account_id(account_id).clear(BlobOp::Link {
                hash: hash.clone(),
                to: BlobLink::Temporary { until },
            });
        }
    }
    batch.clear(BlobOp::Commit { hash: hash.clone() });
    server
        .store()
        .write_batch(&mut batch)
        .await
        .caused_by(trc::location!())
        .map(|_| ())
}

async fn finish(server: &Server, refused: u64) -> trc::Result<bool> {
    let counts = report(server, false).await?;
    let clean = counts.is_zero();
    if clean {
        say!("result: clean, upstream Stalwart can read this data store");
    } else if refused > 0 && counts.chunked_marks == refused && counts.share_link_records == 0 {
        say!(
            "result: {refused} blob(s) refused for size; raise STALWART_FORK_MAX_MEMORY and run again"
        );
    } else {
        say!("result: NOT clean");
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("8M"), Some(8 << 20));
        assert_eq!(parse_size("2 GiB"), Some(2 << 30));
        assert_eq!(parse_size("5x"), None);
    }
}
