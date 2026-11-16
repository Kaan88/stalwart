/*
 * SPDX-FileCopyrightText: 2026 Kaan88 and contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Large request and response bodies backed by chunked blobs.

use crate::{HttpRequest, HttpResponse};
use common::{Server, storage::blob_chunked::parse_byte_range};
use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::{
    StatusCode,
    body::{Bytes, Frame},
};
use std::ops::Range;
use store::{
    BlobStore,
    dispatch::blob_chunked::{CHUNK_SIZE, ChunkManifest, ChunkedBlob, ChunkedBlobWriter},
    write::blob_codec::BlobOpts,
};
use tokio::sync::OwnedSemaphorePermit;

/// A request body read by [`spool_body`].
pub enum SpooledBody {
    /// At most [`CHUNK_SIZE`] bytes, held in memory.
    Memory(Vec<u8>),
    /// Larger bodies, already written to the blob store as chunks. The caller
    /// must commit the blob (`Server::commit_chunked_blob`) or discard it.
    Chunked(ChunkedBlob),
}

impl SpooledBody {
    pub fn len(&self) -> u64 {
        match self {
            SpooledBody::Memory(bytes) => bytes.len() as u64,
            SpooledBody::Chunked(blob) => blob.manifest.size,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Reads a request body. Bodies up to [`CHUNK_SIZE`] stay in memory; larger
/// ones are written to `store` chunk by chunk, so memory use stays at one
/// chunk whatever the size. Returns `Ok(None)` when the body is larger than
/// `max_size` (0 = no limit); chunks already written are deleted then, and
/// when the client breaks off the upload.
pub async fn spool_body(
    req: &mut HttpRequest,
    max_size: u64,
    store: &BlobStore,
    opts: &BlobOpts,
) -> trc::Result<Option<SpooledBody>> {
    let declared = req
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if max_size != 0 && declared.is_some_and(|len| len > max_size) {
        return Ok(None);
    }

    let mut memory: Vec<u8> = Vec::new();
    let mut writer: Option<ChunkedBlobWriter> = None;
    let mut size = 0u64;

    loop {
        let frame = match req.frame().await {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => {
                if let Some(writer) = writer {
                    writer.abort().await;
                }
                return Err(trc::Error::new(trc::EventType::Http(trc::HttpEvent::Error))
                    .details("Request body interrupted")
                    .reason(err));
            }
            None => break,
        };
        let Some(data) = frame.data_ref() else {
            continue;
        };
        size += data.len() as u64;
        if max_size != 0 && size > max_size {
            if let Some(writer) = writer {
                writer.abort().await;
            }
            return Ok(None);
        }

        match &mut writer {
            Some(writer_) => {
                if let Err(err) = writer_.write(data).await {
                    writer.take().unwrap().abort().await;
                    return Err(err);
                }
            }
            None if memory.len() + data.len() <= CHUNK_SIZE => {
                if memory.capacity() == 0
                    && let Some(declared) = declared
                {
                    memory.reserve_exact((declared as usize).min(CHUNK_SIZE));
                }
                memory.extend_from_slice(data);
            }
            None => {
                let mut new_writer = ChunkedBlobWriter::new(store.clone(), opts.clone());
                let result = match new_writer.write(&memory).await {
                    Ok(()) => new_writer.write(data).await,
                    Err(err) => Err(err),
                };
                memory = Vec::new();
                if let Err(err) = result {
                    new_writer.abort().await;
                    return Err(err);
                }
                writer = Some(new_writer);
            }
        }
    }

    match writer {
        Some(writer) => writer
            .finish()
            .await
            .map(|blob| Some(SpooledBody::Chunked(blob))),
        None => Ok(Some(SpooledBody::Memory(memory))),
    }
}

/// A response body that streams `range` of a chunked blob. A task reads the
/// chunks and hands them over through a channel of one, so a download holds
/// about two chunks in memory and reading stops when the client goes away.
/// A read error ends the body early (the client sees a short response against
/// the declared length); the error is logged. `permit` is held until the
/// reading is done.
pub fn chunked_body(
    server: Server,
    manifest: ChunkManifest,
    range: Range<u64>,
    permit: Option<OwnedSemaphorePermit>,
) -> BoxBody<Bytes, hyper::Error> {
    let parts: Vec<(u32, Range<usize>)> = manifest.chunks_for(range).collect();
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(1);
    tokio::spawn(async move {
        let _permit = permit;
        for (index, part) in parts {
            let bytes = match server.blob_chunk(&manifest, index).await {
                Ok(mut chunk) if part.start == 0 && part.end == chunk.len() => {
                    chunk.shrink_to_fit();
                    Bytes::from(chunk)
                }
                Ok(mut chunk) => {
                    chunk.truncate(part.end);
                    Bytes::from(chunk).slice(part.start..)
                }
                Err(err) => {
                    trc::error!(err.details("Failed to stream blob chunk"));
                    break;
                }
            };
            if tx.send(bytes).await.is_err() {
                // The client went away.
                break;
            }
        }
    });
    BoxBody::new(ChannelBody { rx })
}

struct ChannelBody {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        self.rx
            .poll_recv(cx)
            .map(|bytes| bytes.map(|bytes| Ok(Frame::data(bytes))))
    }
}

/// Serves a chunked blob, honouring a single-range `Range` header. `response`
/// carries the headers that do not depend on the range (type, disposition…).
pub fn serve_chunked(
    server: Server,
    manifest: ChunkManifest,
    range_header: Option<&str>,
    response: HttpResponse,
    permit: Option<OwnedSemaphorePermit>,
) -> HttpResponse {
    let total = manifest.size;
    let response = response.with_accept_ranges();
    let range = match range_header.map(|value| parse_byte_range(value, total)) {
        Some(Err(())) => {
            return response
                .with_status_code(StatusCode::RANGE_NOT_SATISFIABLE)
                .with_content_range(format!("bytes */{total}"));
        }
        Some(Ok(Some(range))) => range,
        _ => {
            return response
                .with_content_length(total as usize)
                .with_stream_body(chunked_body(server, manifest, 0..total, permit));
        }
    };
    response
        .with_status_code(StatusCode::PARTIAL_CONTENT)
        .with_content_range(format!("bytes {}-{}/{}", range.start, range.end - 1, total))
        .with_content_length((range.end - range.start) as usize)
        .with_stream_body(chunked_body(server, manifest, range, permit))
}
