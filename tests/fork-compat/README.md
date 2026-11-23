# Fork compatibility test

This test checks that data written by this fork can go back to official Stalwart without loss. Taking it back means running `convert` (everything kept) or `nuke` (big files deleted). The fork's chunked blobs and share links are the only things official Stalwart can't read, and these two tools remove them.

Run it on every rebase onto a new upstream tag.

## The tools

The tools are built into the fork's binary. Stop the server first, then run:

```bash
STALWART_FORK_TOOL=report stalwart --config=/etc/stalwart/config.json
```

The binary loads its configuration, runs the tool and exits. It starts no listener and no background task.

| Tool | What it does |
|---|---|
| `report` | Lists every chunked blob and what uses it (files, mail, pending uploads), and every file with share links. Ends with the counts. |
| `convert` | Rewrites every chunked blob as an ordinary blob, checking it against its hash first, then deletes its chunks and mark. Deletes all share links. Everything else stays. |
| `nuke` | Deletes the files whose content is a chunked blob, the same way their owner deleting them would (quota, change log). Deletes chunked blobs nothing else uses, converts the ones still in use (mail), and deletes all share links. |

Both `convert` and `nuke`:

- **Check their own work.** They end with the `report` counts, which must all be zero.
- **Do nothing the second time.** A repeat run finds nothing left to change.
- **Respect a memory limit.** `STALWART_FORK_MAX_MEMORY` (default `1G`) caps how much a blob may hold in memory while it's rewritten. A larger blob is refused and reported, and the tool exits with an error.
- **Survive interruption.** Before a blob is rewritten, its manifest is saved in the blob's commit record. A run that was killed, crashed or ran out of disk is finished by the next run.

`STALWART_FORK_CRASH_AT=journal|rewrite` is for tests only: it aborts at that step of the first blob.

## Running the test

You need:

- **The fork,** built with RocksDB.
- **Official Stalwart,** built from the pinned tag (v1.1.1) with the same features. Build it in its own target directory: two checkouts sharing one directory overwrite each other's crates.
- **The checker** `tests/fork-compat/inspect`.
- **`stalwart-cli`** on `PATH`.

The `Cargo.lock` must not change the `rocksdb` or `librocksdb-sys` versions compared with the tag.

```bash
git worktree add ../stalwart-official v1.1.1
(cd ../stalwart-official && CARGO_PROFILE_DEV_DEBUG=0 CARGO_TARGET_DIR=../target-official cargo build -p stalwart)
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p stalwart -p fork-compat-inspect
export FORK_BIN=target/debug/stalwart OFFICIAL_BIN=../target-official/debug/stalwart \
       INSPECT_BIN=target/debug/inspect WORK=/tmp/fork-compat
tests/fork-compat/run.sh rocksdb 0 1   # blobs in RocksDB, no encryption, with failure cases
tests/fork-compat/run.sh rocksdb 1 1   # LZ4 and AES-256-GCM on file and mail blobs
tests/fork-compat/run.sh fs 0          # blobs on the file system
tests/fork-compat/run.sh fs 1
sudo -E tests/fork-compat/disk-full.sh 0   # needs a tmpfs mount; after the rocksdb runs
```

## What each step checks

**0. Setup.** A fresh server on RocksDB with two users and a disk quota. The limits are raised so 100 MB uploads pass. With encryption on, a key is created and used for file and mail blobs.

**1. Seed, on the fork.** Over WebDAV:

- files of 4 KiB, exactly 8 MiB, 8 MiB + 1, 20 MiB and 100 MiB + 7, plus an empty file;
- a small and a big file with non-ASCII names;
- the second user's files;
- a WebDAV copy, so two files share one chunked blob.

Over JMAP:

- a 20 MiB upload made into a file, and a 12 MiB upload left unused;
- an email imported from a 14 MB upload with a 10 MiB attachment, so its message blob is chunked;
- a normal mail, a calendar event and a contact.

Two share links are also made. The SHA-256 of every file, JMAP states and quotas go into `state.json`.

**2. Baseline.** The fork's `report` and the independent checker must agree: 8 chunked blobs, 29 chunk keys, 2 files with share links. With encryption on, the checker must also see encrypted blobs.

The checker reads the database directly and doesn't use the fork's code. It counts non-empty blob commit values, 20-byte chunk keys, manifests, property-200 records and encrypted blobs. With blobs on the file system it reads the blob directory.

**3. Convert.**

1. Run `convert`. The tool must end clean, and the checker must count zero fork-only records.
2. Start official Stalwart and check every file over WebDAV and JMAP, the mail's raw message and attachment, the event, the contact and the quotas, all unchanged.
3. Old share links must answer 404.
4. Upload, replace and delete a file.
5. Run official Stalwart's blob purge and check everything again; the checker must still count zero.
6. Restart twice, compact every column family, and check everything again with no corruption or compaction errors in the log.
7. Start the fork on the converted data and check it still serves everything.
8. Run `convert` a second time; it must do nothing.

**4. Nuke.**

1. Get a reference first: on converted data, official Stalwart deletes the big files by hand over WebDAV.
2. Run `nuke`. It must delete exactly the 7 big files, keep and convert the mail, and end clean.
3. On official Stalwart:
   - every small file, the mail, the event and the contact must be intact;
   - each user's quota must equal the reference exactly (a file's quota covers its content and its metadata record);
   - `FileNode/changes` must list exactly the deleted files as destroyed.
4. Purge, restart and compact as in step 3.
5. Run `nuke` a second time; it must do nothing.

**5. Failure cases,** on the RocksDB runs (`run.sh ... 1`):

- **Kill.** The convert is killed with `kill -9` at three moments, then run again. It must finish and pass all checks.
- **Crash mid-blob.** The convert is aborted after a blob's journal entry is written, and again after the blob is rewritten whole but before its chunks are deleted. The report must show the blob mid-conversion, and the next run must finish it.
- **Memory limit.** With a 50 MiB limit, the convert refuses exactly the 100 MiB file and exits with an error. With the default limit it then finishes.
- **Disk full** (`disk-full.sh`). The baseline goes on a small tmpfs with about 60 MB left. The convert must stop with an error and not claim success. Once space is freed, it must finish, and official Stalwart must serve everything.

## Pass criteria

- After `convert`, official Stalwart on RocksDB serves every file and attachment byte for byte.
- After `nuke`, only the big files are gone and everything else is intact.
- The tool and the checker both count zero fork-only records.
- Quotas and change logs add up.
- Official Stalwart restarts, compacts and purges with no errors in its log.
