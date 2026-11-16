# What this fork adds

This is Stalwart v1.1.1 with two additions for file storage. Everything else is upstream, unchanged.

1. **Big files.** Files over 8 MiB are stored in 8 MiB chunks, and are streamed in and out instead of being held in memory.
2. **Public share links.** Anyone with the link can download a file, Google Drive style. Every link expires.

The CI (`.github/workflows/docker-arm64.yml`) builds only an arm64 image and pushes it to this repository's GHCR, tagged `arm64` and `arm64-<sha>`.

## Big files

Upstream reads every upload into memory and reads a whole blob into memory for every download, even for a range request. With this fork:

- **Uploads.** A WebDAV `PUT` or a JMAP upload (`/jmap/upload/{accountId}/`) larger than 8 MiB goes to the blob store one chunk at a time. Memory use stays at one chunk, whatever the file size. Smaller bodies take the old path.
- **Downloads.** WebDAV `GET`, JMAP download (`/jmap/download/...`) and share links stream a chunked file one chunk at a time, and support `Range`. JMAP download did not support ranges before.
- **Other readers.** Anything else that reads a blob (search indexing, backup, `Blob/get`) still gets the whole content as before, put together from the chunks.
- **Storage format.** Each chunk is compressed and encrypted on its own, with the configured blob settings. A small manifest is stored under the blob's hash. The main store marks a chunked blob with the value `[1]` on its `BlobOp::Commit` entry; the blob purge uses that mark to delete the chunks with the manifest.
- **Size cap.** `FileNode` stores the size as a `u32`, so a file can be at most 4 GiB − 1 byte. Changing that would change the on-disk format of every file, so it is left as it is.

### Settings to raise

Big files are refused until these limits are raised:

| Setting | Upstream default | Used by |
|---|---|---|
| File storage → maximum file size (`FileStorage.maxSize`) | 25 MiB | WebDAV `PUT` |
| JMAP → maximum upload size (`Jmap.maxUploadSize`) | see your server | JMAP upload |
| JMAP → upload quota (`uploadTmpQuotaSize`) | see your server | JMAP upload (per account, per period) |

Users with the `UnlimitedUploads` permission skip the size limits.

### Known limits

- **Blob recoding.** Changing compression or encryption of existing blobs skips chunked blobs. Their chunks keep the settings they were written with.
- **Interrupted uploads.** If the server stops in the middle of an upload, the chunks already written stay in the blob store. No index entry points to them, so nothing reads or deletes them.
- **Tenant quota.** The tenant quota check on WebDAV `PUT` (Enterprise only, in licence-restricted code this fork leaves alone) does not count a body over 8 MiB. The per-account disk quota still counts it, because it uses the file size.
- **Backups.** A backup stores a chunked file as one blob, and a restore writes it back unchunked.

## Share links

A link points to one file, not a folder. It always expires: after 7 days by default, at least 1 hour, at most 365 days. A file can have at most 20 live links.

Only the owner of the file can manage its links (or, for a group account, its members).

- **Where links live.** They are stored with the file, under property 200 of its FileNode document. Deleting the file or the account deletes its links.
- **Token.** It is 56 hex characters: account id, file id and link id, then a 128-bit random secret. The server finds the link from the token alone and compares the secret in constant time.
- **Logs.** The server's request-URL log shows `/s/[token]` instead of the token.

### Download: `GET /s/<token>` (no sign-in)

The response is always an attachment, with `nosniff`, a sandbox CSP, `no-store`, `noindex` and `no-referrer`, so a shared HTML file can't run on your domain. It supports `Range` and `HEAD`.

- **Rate limits.** The anonymous rate limit applies, and at most 32 downloads run at once across all links. More get `503` with `Retry-After`.
- **Unknown links.** An unknown, revoked or expired link gets `404`, and the response doesn't say which.

### Management: `POST /api/sharelink/<action>` (signed in)

Requests take a JSON body. Use the same credentials as JMAP; an OAuth token needs the files scope.

| Action | Body | Answer |
|---|---|---|
| `create` | `{"fileId": "…", "expiresIn": 604800}` (`expiresIn` in seconds, optional) | `{"id", "fileId", "path", "url", "created", "expires"}` |
| `list` | `{"fileId": "…"}` | an array of the same objects, live links only |
| `revoke` | `{"fileId": "…", "linkId": "…"}` | `{"revoked": true\|false}` |
| `limits` | `{}` | `{"defaultExpiresIn", "minExpiresIn", "maxExpiresIn", "maxLinksPerFile"}` |

- **Fields.** `fileId` is the JMAP FileNode id. `accountId` is optional and defaults to your own account. Times are Unix seconds.
- **URL.** `url` is built from the request's `Host` header. It uses `https` when the connection is TLS or `X-Forwarded-Proto` says `https`. A client that knows the server's address can use `path` instead.
- **Permissions.** `create` and `revoke` need the `JmapFileNodeUpdate` permission; `list` needs `JmapFileNodeGet`.

## Where the code is

New files, all AGPL-3.0-only:

- `crates/store/src/dispatch/blob_chunked.rs`: chunk format, writer, reads, deletion
- `crates/common/src/storage/blob_chunked.rs`: server helpers, the commit of an upload, `Range` parsing
- `crates/http-proto/src/blob.rs`: streaming request and response bodies
- `crates/groupware/src/file/share_link.rs`: link storage and tokens
- `crates/http/src/sharelink.rs`: the `/s/` and `/api/sharelink` handlers

Small hooks in upstream files:

- **Store layer:** the blob read path and the blob purge (`store`), and the recode task (`services`).
- **Uploads and downloads:** JMAP upload and download (`http/src/request.rs`, `jmap/src/blob/upload.rs`); WebDAV `PUT` and `GET` (`dav`, `dav-proto`).
- **Share links:** the delete paths of file nodes (`groupware/src/file/storage.rs`), the router, and the request-URL log.

Unit tests cover the manifest, chunk ranges, `Range` parsing and share tokens. Run them with:

```bash
cargo test -p store --no-default-features --features sqlite --lib blob_chunked
```

### Checked

These were run against a local server (SQLite store) built from this code:

- **WebDAV.** `PUT`, `GET` and replace of a 20 MiB file (3 chunks), with a range across a chunk boundary. A `PUT` over the limit gets 413 and leaves no chunks behind; a `PUT` the client breaks off halfway also leaves none.
- **JMAP.** Upload of 20 MiB, `FileNode/set` with the blob, and download with a suffix range and an unsatisfiable range (416).
- **Share links: downloading.** A public download matches the file byte for byte, both for a chunked file and for a small file served from memory. The headers, ranges and `HEAD` are as described above. A wrong secret or a garbage token gets 404.
- **Share links: managing.** Listing works, and is refused without sign-in. The expiry is clamped. A revoked link gets 404 while the file's other links keep working, and every link of a deleted file gets 404. A folder cannot be linked.
- **Logs.** The token never appears in the server log.
- **Purge.** The blob purge deletes the chunks and the manifest of an unlinked chunked upload once its hold expires, and leaves linked files whole.
- **Concurrent downloads.** After 40 public downloads were cancelled, a new one still works: download slots are given back.

The workspace also compiles with the CI feature set (`sqlite postgres mysql s3 redis keep azure nats enterprise`). RocksDB and FoundationDB were not built locally.
