/*
 * SPDX-FileCopyrightText: 2026 Kaan88 and contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Public share links for files ("anyone with the link can download").
//!
//! The links of a file are kept with the file, as a small JSON list under a
//! property of its FileNode document, so they move with it, are deleted with
//! it (see `storage.rs`) and with its account. A link's token carries the
//! account, file and link ids followed by a random secret, so the public route
//! finds the link without any global index; the secret is compared in
//! constant time. Every link expires.

use common::Server;
use serde::{Deserialize, Serialize};
use store::{
    ValueKey,
    write::{BatchBuilder, ValueClass, now, serialize::RawValue},
};
use trc::AddContext;
use types::collection::Collection;

/// Property of a FileNode document that holds its share links.
/// FileNode documents only use the archive property (50) upstream.
pub const SHARE_LINKS_FIELD: u8 = 200;
pub const MAX_LINKS_PER_FILE: usize = 20;
pub const DEFAULT_LINK_EXPIRY: u64 = 7 * 86_400;
pub const MIN_LINK_EXPIRY: u64 = 3_600;
pub const MAX_LINK_EXPIRY: u64 = 365 * 86_400;

const SECRET_LEN: usize = 16;
const TOKEN_LEN: usize = 12 + SECRET_LEN;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLink {
    pub id: u32,
    /// Hex-encoded random secret, part of the token.
    pub secret: String,
    pub created: u64,
    pub expires: u64,
    pub created_by: u32,
}

/// The parts of a share token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareToken {
    pub account_id: u32,
    pub document_id: u32,
    pub link_id: u32,
    pub secret: [u8; SECRET_LEN],
}

impl ShareToken {
    pub fn parse(token: &str) -> Option<Self> {
        let bytes = decode_hex(token)?;
        if bytes.len() != TOKEN_LEN {
            return None;
        }
        Some(ShareToken {
            account_id: u32::from_be_bytes(bytes[0..4].try_into().ok()?),
            document_id: u32::from_be_bytes(bytes[4..8].try_into().ok()?),
            link_id: u32::from_be_bytes(bytes[8..12].try_into().ok()?),
            secret: bytes[12..].try_into().ok()?,
        })
    }
}

impl ShareLink {
    pub fn new(created_by: u32, expires_in: u64, existing: &[ShareLink]) -> Self {
        let mut id = 0;
        while id == 0 || existing.iter().any(|link| link.id == id) {
            id = rand_u32();
        }
        let mut secret = [0u8; SECRET_LEN];
        store::rand::fill(&mut secret);
        let created = now();
        ShareLink {
            id,
            secret: encode_hex(&secret),
            created,
            expires: created + expires_in.clamp(MIN_LINK_EXPIRY, MAX_LINK_EXPIRY),
            created_by,
        }
    }

    pub fn token(&self, account_id: u32, document_id: u32) -> String {
        let mut token = String::with_capacity(TOKEN_LEN * 2);
        token.push_str(&encode_hex(&account_id.to_be_bytes()));
        token.push_str(&encode_hex(&document_id.to_be_bytes()));
        token.push_str(&encode_hex(&self.id.to_be_bytes()));
        token.push_str(&self.secret);
        token
    }

    pub fn is_expired(&self, now: u64) -> bool {
        self.expires <= now
    }

    /// Constant-time comparison of the link's secret with a token's.
    pub fn secret_matches(&self, secret: &[u8; SECRET_LEN]) -> bool {
        let Some(own) = decode_hex(&self.secret) else {
            return false;
        };
        own.len() == SECRET_LEN
            && own
                .iter()
                .zip(secret.iter())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }
}

fn links_key(account_id: u32, document_id: u32) -> ValueKey<ValueClass> {
    ValueKey {
        account_id,
        collection: Collection::FileNode.into(),
        document_id,
        class: ValueClass::Property(SHARE_LINKS_FIELD),
    }
}

/// The links of a file, expired ones included.
pub async fn file_share_links(
    server: &Server,
    account_id: u32,
    document_id: u32,
) -> trc::Result<Vec<ShareLink>> {
    match server
        .store()
        .get_value::<RawValue>(links_key(account_id, document_id))
        .await
        .caused_by(trc::location!())?
    {
        Some(raw) => serde_json::from_slice(&raw.0).map_err(|err| {
            trc::StoreEvent::DataCorruption
                .into_err()
                .details("Invalid share links")
                .reason(err)
                .caused_by(trc::location!())
        }),
        None => Ok(Vec::new()),
    }
}

/// Replaces the links of a file, dropping expired ones.
pub async fn write_file_share_links(
    server: &Server,
    account_id: u32,
    document_id: u32,
    mut links: Vec<ShareLink>,
) -> trc::Result<()> {
    let now = now();
    links.retain(|link| !link.is_expired(now));
    let mut batch = BatchBuilder::new();
    batch
        .with_account_id(account_id)
        .with_collection(Collection::FileNode)
        .with_document(document_id);
    if links.is_empty() {
        batch.clear(ValueClass::Property(SHARE_LINKS_FIELD));
    } else {
        let bytes = serde_json::to_vec(&links).map_err(|err| {
            trc::StoreEvent::UnexpectedError
                .into_err()
                .reason(err)
                .caused_by(trc::location!())
        })?;
        batch.set(ValueClass::Property(SHARE_LINKS_FIELD), bytes);
    }
    server
        .store()
        .write_batch(&mut batch)
        .await
        .caused_by(trc::location!())
        .map(|_| ())
}

/// Finds the live link a token names. `None` for an unknown, revoked or
/// expired link or a wrong secret, without saying which.
pub async fn resolve_share_token(
    server: &Server,
    token: &ShareToken,
) -> trc::Result<Option<ShareLink>> {
    let now = now();
    Ok(
        file_share_links(server, token.account_id, token.document_id)
            .await?
            .into_iter()
            .find(|link| {
                link.id == token.link_id
                    && link.secret_matches(&token.secret)
                    && !link.is_expired(now)
            }),
    )
}

fn rand_u32() -> u32 {
    let mut bytes = [0u8; 4];
    store::rand::fill(&mut bytes);
    u32::from_be_bytes(bytes)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trip() {
        let link = ShareLink::new(5, DEFAULT_LINK_EXPIRY, &[]);
        let token = link.token(7, 42);
        assert_eq!(token.len(), TOKEN_LEN * 2);
        let parsed = ShareToken::parse(&token).unwrap();
        assert_eq!(parsed.account_id, 7);
        assert_eq!(parsed.document_id, 42);
        assert_eq!(parsed.link_id, link.id);
        assert!(link.secret_matches(&parsed.secret));

        let mut other = parsed.secret;
        other[0] ^= 1;
        assert!(!link.secret_matches(&other));
    }

    #[test]
    fn bad_tokens() {
        assert_eq!(ShareToken::parse(""), None);
        assert_eq!(ShareToken::parse("zz"), None);
        assert_eq!(ShareToken::parse(&"a".repeat(TOKEN_LEN * 2 - 1)), None);
        assert_eq!(ShareToken::parse(&"a".repeat(TOKEN_LEN * 2 + 2)), None);
        assert!(ShareToken::parse(&"a".repeat(TOKEN_LEN * 2)).is_some());
        assert_eq!(ShareToken::parse(&"é".repeat(TOKEN_LEN)), None);
    }

    #[test]
    fn expiry_is_clamped() {
        let short = ShareLink::new(1, 1, &[]);
        assert_eq!(short.expires - short.created, MIN_LINK_EXPIRY);
        let long = ShareLink::new(1, u64::MAX / 2, &[]);
        assert_eq!(long.expires - long.created, MAX_LINK_EXPIRY);
        assert!(!long.is_expired(long.created));
        assert!(long.is_expired(long.expires));
    }

    #[test]
    fn link_ids_are_unique() {
        let mut links = Vec::new();
        for _ in 0..50 {
            let link = ShareLink::new(1, DEFAULT_LINK_EXPIRY, &links);
            assert!(
                links
                    .iter()
                    .all(|l: &ShareLink| l.id != link.id && link.id != 0)
            );
            links.push(link);
        }
    }
}
