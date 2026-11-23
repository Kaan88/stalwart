/*
 * SPDX-FileCopyrightText: 2026 Kaan88 and contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Read-only checker for the fork-compatibility tests.
//!
//! Counts the records only the fork writes, straight from a RocksDB data
//! store, without any of the fork's code, so the fork's own `report` is not
//! the only witness:
//!
//! * commit records (`k` column family, 32-byte keys) with a non-empty value;
//! * 20-byte keys in the blob column family (`t`): chunk keys;
//! * values in `t` that look like a chunk manifest (57 bytes, `STWCHNK1`
//!   prefix, `0xc3` last byte);
//! * share-link records: property 200 of collection 12 (`p` column family,
//!   key = account(4) collection(1) field(1) document(4)).
//!
//! With blobs on the file system (`--blobs <dir>`), blobs are counted in that
//! directory instead: a chunk key is a file whose name is 32 characters (20
//! key bytes, base32), a manifest a 57-byte file with the prefix and marker.
//!
//! Usage: `inspect [--blobs <blob dir>] <data dir>` (read-only; the server must
//! be stopped) or `inspect --compact <data dir>` (compacts every column family).

use rocksdb::{DB, Options};
use std::process::ExitCode;

const CF_BLOB_LINKS: &str = "k";
const CF_BLOBS: &str = "t";
const CF_PROPERTY: &str = "p";
const HASH_LEN: usize = 32;
const CHUNK_KEY_LEN: usize = 20;
const MANIFEST_LEN: usize = 57;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (compact, blob_dir, path) = match args.as_slice() {
        [flag, path] if flag == "--compact" => (true, None, path),
        [flag, dir, path] if flag == "--blobs" => (false, Some(dir.clone()), path),
        [path] => (false, None, path),
        _ => {
            eprintln!("usage: inspect [--compact | --blobs <dir>] <rocksdb data dir>");
            return ExitCode::from(2);
        }
    };
    let opts = Options::default();
    let cfs = match DB::list_cf(&opts, path) {
        Ok(cfs) => cfs,
        Err(err) => {
            eprintln!("cannot list column families in {path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    if compact {
        let db = match DB::open_cf(&opts, path, &cfs) {
            Ok(db) => db,
            Err(err) => {
                eprintln!("cannot open {path}: {err}");
                return ExitCode::FAILURE;
            }
        };
        for name in &cfs {
            if let Some(cf) = db.cf_handle(name) {
                db.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);
            }
        }
        println!("compacted {} column families", cfs.len());
        return ExitCode::SUCCESS;
    }

    let db = match DB::open_cf_for_read_only(&opts, path, &cfs, false) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("cannot open {path} read-only: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut commit_entries = 0u64;
    let mut commit_values_nonempty = 0u64;
    if let Some(cf) = db.cf_handle(CF_BLOB_LINKS) {
        for item in db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (key, value) = match item {
                Ok(kv) => kv,
                Err(err) => {
                    eprintln!("read error: {err}");
                    return ExitCode::FAILURE;
                }
            };
            if key.len() == HASH_LEN {
                commit_entries += 1;
                if !value.is_empty() {
                    commit_values_nonempty += 1;
                }
            }
        }
    }

    let mut blobs = 0u64;
    let mut blob_bytes = 0u64;
    let mut chunk_keys = 0u64;
    let mut manifests = 0u64;
    let mut encrypted_blobs = 0u64;
    let mut count = |key_len: usize, value: &[u8]| {
        blobs += 1;
        blob_bytes += value.len() as u64;
        if key_len == CHUNK_KEY_LEN {
            chunk_keys += 1;
        }
        if value.len() == MANIFEST_LEN
            && value.starts_with(b"STWCHNK1")
            && value.last() == Some(&0xc3)
        {
            manifests += 1;
        }
        // Blob codec trailer: 0xa0 marker with the 0x10 "encrypted" bit.
        if value.last().is_some_and(|b| b & 0xf0 == 0xb0) {
            encrypted_blobs += 1;
        }
    };
    if let Some(dir) = &blob_dir {
        let mut stack = vec![std::path::PathBuf::from(dir)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(value) = std::fs::read(&path) {
                    let name_len = path.file_name().map_or(0, |n| n.len());
                    // base32: 20 key bytes are 32 characters, 32 bytes 52.
                    count(
                        if name_len == 32 {
                            CHUNK_KEY_LEN
                        } else {
                            name_len
                        },
                        &value,
                    );
                }
            }
        }
    } else if let Some(cf) = db.cf_handle(CF_BLOBS) {
        for item in db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (key, value) = match item {
                Ok(kv) => kv,
                Err(err) => {
                    eprintln!("read error: {err}");
                    return ExitCode::FAILURE;
                }
            };
            count(key.len(), &value);
        }
    }
    drop(count);

    let mut share_link_records = 0u64;
    let mut file_archives = 0u64;
    if let Some(cf) = db.cf_handle(CF_PROPERTY) {
        for item in db.iterator_cf(&cf, rocksdb::IteratorMode::Start) {
            let (key, _) = match item {
                Ok(kv) => kv,
                Err(err) => {
                    eprintln!("read error: {err}");
                    return ExitCode::FAILURE;
                }
            };
            if key.len() == 10 && key[4] == 12 {
                match key[5] {
                    200 => share_link_records += 1,
                    50 => file_archives += 1,
                    _ => {}
                }
            }
        }
    }

    println!(
        "inspect: commit_values_nonempty={commit_values_nonempty} manifests={manifests} \
         chunk_keys={chunk_keys} share_link_records={share_link_records} \
         commit_entries={commit_entries} blobs={blobs} blob_bytes={blob_bytes} \
         encrypted_blobs={encrypted_blobs} file_archives={file_archives}"
    );
    ExitCode::SUCCESS
}
