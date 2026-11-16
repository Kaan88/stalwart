/*
 * SPDX-FileCopyrightText: 2026 Kaan88 and contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Public share links: `GET /s/<token>` downloads a shared file without
//! signing in; `POST /api/sharelink/{create,list,revoke}` manages links.

use common::{
    Server, auth::AccessToken, auth::oauth::scope::OAuthScopes,
    storage::blob_chunked::parse_byte_range,
};
use groupware::file::{
    FileNode,
    share_link::{
        DEFAULT_LINK_EXPIRY, MAX_LINK_EXPIRY, MAX_LINKS_PER_FILE, MIN_LINK_EXPIRY, ShareLink,
        ShareToken, file_share_links, resolve_share_token, write_file_share_links,
    },
};
use http_proto::{HttpRequest, HttpResponse, HttpSessionData, blob::serve_chunked};
use hyper::{Method, StatusCode, header};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use registry::schema::enums::Permission;
use serde::{Deserialize, Serialize};
use std::{
    str::FromStr,
    sync::{Arc, LazyLock},
};
use store::{
    ValueKey,
    write::{Archive, ArchiveBytes, now},
};
use tokio::sync::Semaphore;
use trc::AddContext;
use types::{blob_hash::BlobHash, collection::Collection, id::Id};

/// Public downloads served at the same time, across all links. Each one holds
/// at most one chunk (or one small file) in memory.
const MAX_PUBLIC_DOWNLOADS: usize = 32;

static PUBLIC_DOWNLOADS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_PUBLIC_DOWNLOADS)));

/// The request URL as it may be logged: share tokens are secrets.
pub fn loggable_url(uri: &hyper::Uri) -> compact_str::CompactString {
    if uri.path().starts_with("/s/") {
        compact_str::CompactString::const_new("/s/[token]")
    } else {
        compact_str::ToCompactString::to_compact_string(uri)
    }
}

fn not_found() -> HttpResponse {
    HttpResponse::new(StatusCode::NOT_FOUND)
        .with_content_type("text/plain; charset=utf-8")
        .with_no_store()
        .with_header("X-Robots-Tag", "noindex, nofollow")
        .with_text_body("This link does not exist or has expired.\n")
}

pub async fn handle_share_download(
    server: &Server,
    req: &HttpRequest,
    session: &HttpSessionData,
    token: &str,
) -> trc::Result<HttpResponse> {
    server
        .is_http_anonymous_request_allowed(session.remote_ip)
        .await?;

    let is_head = match *req.method() {
        Method::GET => false,
        Method::HEAD => true,
        _ => return Ok(HttpResponse::new(StatusCode::METHOD_NOT_ALLOWED)),
    };
    let Some(token) = ShareToken::parse(token) else {
        return Ok(not_found());
    };
    if resolve_share_token(server, &token).await?.is_none() {
        return Ok(not_found());
    }

    // The file as it is now (it may have been renamed or replaced).
    let Some(node_) = server
        .store()
        .get_value::<Archive<ArchiveBytes>>(ValueKey::archive(
            token.account_id,
            Collection::FileNode,
            token.document_id,
        ))
        .await
        .caused_by(trc::location!())?
    else {
        return Ok(not_found());
    };
    let node = node_.unarchive::<FileNode>().caused_by(trc::location!())?;
    let Some(file) = node.file() else {
        return Ok(not_found());
    };
    let hash = BlobHash::from(&file.blob_hash);
    let size = u64::from(u32::from(file.size));
    let content_type = file
        .media_type
        .as_ref()
        .map(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    let name = node.name.as_str();

    let response = HttpResponse::new(StatusCode::OK)
        .with_content_type(content_type)
        .with_content_disposition(content_disposition(name))
        .with_sandbox_policy()
        .with_no_store()
        .with_header("X-Robots-Tag", "noindex, nofollow")
        .with_header(header::REFERRER_POLICY, "no-referrer")
        .with_accept_ranges();

    if is_head {
        return Ok(response.with_content_length(size as usize));
    }

    let Ok(permit) = PUBLIC_DOWNLOADS.clone().try_acquire_owned() else {
        return Ok(HttpResponse::new(StatusCode::SERVICE_UNAVAILABLE)
            .with_header(header::RETRY_AFTER, "30")
            .with_no_store());
    };
    let range_header = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());

    if let Some(manifest) = server.chunk_manifest(&hash).await? {
        return Ok(serve_chunked(
            server.clone(),
            manifest,
            range_header,
            response,
            Some(permit),
        ));
    }

    let Some(contents) = server
        .fetch_blob(&hash, 0..usize::MAX, Some(size))
        .await
        .caused_by(trc::location!())?
    else {
        return Ok(not_found());
    };
    let (contents, _reservation) = contents.into_parts();
    let total = contents.len() as u64;
    Ok(
        match range_header.map(|value| parse_byte_range(value, total)) {
            Some(Err(())) => response
                .with_status_code(StatusCode::RANGE_NOT_SATISFIABLE)
                .with_content_range(format!("bytes */{total}")),
            Some(Ok(Some(range))) => response
                .with_status_code(StatusCode::PARTIAL_CONTENT)
                .with_content_range(format!("bytes {}-{}/{}", range.start, range.end - 1, total))
                .with_binary_body(contents[range.start as usize..range.end as usize].to_vec()),
            _ => response.with_binary_body(contents),
        },
    )
}

/// `attachment` with an ASCII fallback name and the full UTF-8 name (RFC 6266).
fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        utf8_percent_encode(name, NON_ALPHANUMERIC)
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkRequest {
    /// The account that holds the file; the caller's own when missing.
    account_id: Option<String>,
    /// The FileNode id (as JMAP gives it).
    file_id: Option<String>,
    /// Link id, for `revoke`.
    link_id: Option<String>,
    /// Seconds until the link expires, for `create`.
    expires_in: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkView {
    id: String,
    file_id: String,
    path: String,
    url: String,
    created: u64,
    expires: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkLimits {
    default_expires_in: u64,
    min_expires_in: u64,
    max_expires_in: u64,
    max_links_per_file: usize,
}

fn bad_request(details: &'static str) -> trc::Error {
    trc::ResourceEvent::BadParameters
        .into_err()
        .details(details)
}

fn view(link: &ShareLink, account_id: u32, document_id: u32, base_url: &str) -> LinkView {
    let path = format!("/s/{}", link.token(account_id, document_id));
    LinkView {
        id: link.id.to_string(),
        file_id: Id::from(document_id).to_string(),
        url: format!("{base_url}{path}"),
        path,
        created: link.created,
        expires: link.expires,
    }
}

fn json_response(value: &impl Serialize) -> trc::Result<HttpResponse> {
    serde_json::to_string(value)
        .map(|body| {
            HttpResponse::new(StatusCode::OK)
                .with_content_type("application/json; charset=utf-8")
                .with_no_store()
                .with_text_body(body)
        })
        .map_err(|err| trc::EventType::Resource(trc::ResourceEvent::Error).from_json_error(err))
}

/// `POST /api/sharelink/<action>` for a signed-in user. Only the owner of a
/// file (or a member of the group that owns it) may manage its links.
pub async fn handle_share_api(
    server: &Server,
    action: &str,
    body: Option<Vec<u8>>,
    access_token: AccessToken,
    base_url: &str,
) -> trc::Result<HttpResponse> {
    let access_token = access_token.assert_allows_oauth_scopes(OAuthScopes::FILES)?;

    if action == "limits" {
        return json_response(&LinkLimits {
            default_expires_in: DEFAULT_LINK_EXPIRY,
            min_expires_in: MIN_LINK_EXPIRY,
            max_expires_in: MAX_LINK_EXPIRY,
            max_links_per_file: MAX_LINKS_PER_FILE,
        });
    }

    let request: LinkRequest = match body.as_deref() {
        Some(body) if !body.is_empty() => serde_json::from_slice(body).map_err(|err| {
            trc::EventType::Resource(trc::ResourceEvent::BadParameters).from_json_error(err)
        })?,
        _ => return Err(bad_request("A JSON body is required.")),
    };

    let account_id = match &request.account_id {
        Some(id) => Id::from_str(id)
            .map_err(|_| bad_request("Invalid accountId."))?
            .document_id(),
        None => access_token.account_id(),
    };
    if !access_token.is_member(account_id) {
        return Err(trc::ResourceEvent::NotFound.into_err());
    }
    let file_id = request
        .file_id
        .as_deref()
        .map(|id| {
            Id::from_str(id)
                .map(|id| id.document_id())
                .map_err(|_| bad_request("Invalid fileId."))
        })
        .transpose()?;

    match action {
        "create" => {
            access_token.enforce_permission(Permission::JmapFileNodeUpdate)?;
            let document_id = file_id.ok_or_else(|| bad_request("fileId is required."))?;
            ensure_file(server, account_id, document_id).await?;

            let mut links = file_share_links(server, account_id, document_id).await?;
            let now = now();
            links.retain(|link| !link.is_expired(now));
            if links.len() >= MAX_LINKS_PER_FILE {
                return Err(trc::LimitEvent::TooManyRequests
                    .into_err()
                    .details("Too many links for this file."));
            }
            let link = ShareLink::new(
                access_token.account_id(),
                request.expires_in.unwrap_or(DEFAULT_LINK_EXPIRY),
                &links,
            );
            let result = view(&link, account_id, document_id, base_url);
            links.push(link);
            write_file_share_links(server, account_id, document_id, links).await?;
            json_response(&result)
        }
        "list" => {
            access_token.enforce_permission(Permission::JmapFileNodeGet)?;
            let document_id = file_id.ok_or_else(|| bad_request("fileId is required."))?;
            let now = now();
            let links = file_share_links(server, account_id, document_id)
                .await?
                .iter()
                .filter(|link| !link.is_expired(now))
                .map(|link| view(link, account_id, document_id, base_url))
                .collect::<Vec<_>>();
            json_response(&links)
        }
        "revoke" => {
            access_token.enforce_permission(Permission::JmapFileNodeUpdate)?;
            let document_id = file_id.ok_or_else(|| bad_request("fileId is required."))?;
            let link_id = request
                .link_id
                .as_deref()
                .and_then(|id| id.parse::<u32>().ok())
                .ok_or_else(|| bad_request("linkId is required."))?;
            let mut links = file_share_links(server, account_id, document_id).await?;
            let before = links.len();
            links.retain(|link| link.id != link_id);
            let revoked = links.len() != before;
            if revoked {
                write_file_share_links(server, account_id, document_id, links).await?;
            }
            json_response(&serde_json::json!({ "revoked": revoked }))
        }
        _ => Err(trc::ResourceEvent::NotFound.into_err()),
    }
}

async fn ensure_file(server: &Server, account_id: u32, document_id: u32) -> trc::Result<()> {
    let node_ = server
        .store()
        .get_value::<Archive<ArchiveBytes>>(ValueKey::archive(
            account_id,
            Collection::FileNode,
            document_id,
        ))
        .await
        .caused_by(trc::location!())?
        .ok_or_else(|| trc::ResourceEvent::NotFound.into_err())?;
    let node = node_.unarchive::<FileNode>().caused_by(trc::location!())?;
    if node.file().is_some() {
        Ok(())
    } else {
        Err(bad_request("Only files can be shared by link."))
    }
}

#[cfg(test)]
mod tests {
    use super::content_disposition;

    #[test]
    fn disposition_escapes_names() {
        assert_eq!(
            content_disposition("report.pdf"),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report%2Epdf"
        );
        let header = content_disposition("a\"b\\c\r\nd é.txt");
        assert!(header.starts_with("attachment; filename=\"a_b_c__d _.txt\";"));
        assert!(!header.contains('\r') && !header.contains('\n'));
    }
}
