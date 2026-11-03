//! The opaque-identity registry, as the `opaque` method sees it
//! (`docs/caller-identity#registry`).
//!
//! The registry's storage is the daemon's (`identities` table of
//! `<data_dir>/janus.db`, loaded into memory at startup and written through
//! on every mutation); the `opaque` extractor is `janus_core`'s. This module
//! is where they meet: [`OpaqueRegistry`] is the hot-path probe the
//! extractor calls, [`RegistryEntry`] the stored row, and [`IdentityRecord`]
//! the metadata the admin surface and CLI exchange. Create, list, show,
//! revoke and delete are daemon-internal operations of the store and the
//! admin surface and are not part of this trait.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::identity::ExtractionError;

/// Random bytes in a daemon-generated opaque token, before base64url
/// encoding. The daemon generates the token so no deployment can register a
/// guessable one.
pub const OPAQUE_TOKEN_BYTES: usize = 32;

/// SHA-256 of a token. The only form in which a token is stored or looked
/// up; a leaked database yields nothing a caller could present.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Hash the token exactly as presented (the source's scheme prefix, if
    /// any, already stripped).
    pub fn of(token: &[u8]) -> Self {
        TokenHash(Sha256::digest(token).into())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        TokenHash(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, the `token_hash` column's text form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out)?;
        Ok(TokenHash(out))
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenHash({})", self.to_hex())
    }
}

/// One registry entry's metadata: `GET /v1/identities/{id}` and each item
/// of the list. Never includes the token or its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityRecord {
    /// Server-assigned; entries are addressed by it, never by the token.
    pub id: String,
    /// Becomes `caller_identity`. Several live entries may share one
    /// identity (a rotation in progress).
    pub identity: String,
    /// Becomes `identity_claims`.
    #[serde(default)]
    pub claims: BTreeMap<String, String>,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Set by revoke; effective on the next request.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Approximate by up to one flush interval.
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl IdentityRecord {
    /// Whether this entry verifies at `now`. Revocation is checked first;
    /// an expired entry fails exactly as a revoked one does, with the
    /// distinction kept for the request log.
    pub fn verdict(&self, now: DateTime<Utc>) -> Result<(), ExtractionError> {
        if self.revoked_at.is_some_and(|at| at <= now) {
            return Err(ExtractionError::Revoked);
        }
        if self.expires_at.is_some_and(|at| at <= now) {
            return Err(ExtractionError::Expired);
        }
        Ok(())
    }
}

/// One row of the `identities` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub token_hash: TokenHash,
    pub record: IdentityRecord,
}

/// The in-memory index the `opaque` extractor probes. Implemented by the
/// daemon's store; a lookup is a hash-map probe and never a disk read.
pub trait OpaqueRegistry: Send + Sync {
    /// The entry whose token hashes to `token_hash`, revoked and expired
    /// ones included (the extractor applies [`IdentityRecord::verdict`] so
    /// the request log can tell them apart). On a hit, records `now` as the
    /// entry's `last_seen_at` in memory; the store flushes it periodically.
    /// `None`: no such token, which the extractor reports as `invalid`.
    fn lookup(&self, token_hash: &TokenHash, now: DateTime<Utc>) -> Option<IdentityRecord>;
}

impl fmt::Debug for dyn OpaqueRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OpaqueRegistry")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0)
            .single()
            .expect("valid timestamp")
    }

    fn record() -> IdentityRecord {
        IdentityRecord {
            id: "id-1".into(),
            identity: "sandbox-7".into(),
            claims: BTreeMap::from([("tenant".to_owned(), "acme".to_owned())]),
            label: None,
            created_at: at(1_000),
            expires_at: Some(at(2_000)),
            revoked_at: None,
            last_seen_at: None,
        }
    }

    #[test]
    fn token_hash_is_sha256_and_round_trips_hex() {
        // SHA-256("abc"), FIPS 180-2 test vector.
        let h = TokenHash::of(b"abc");
        assert_eq!(
            h.to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(TokenHash::from_hex(&h.to_hex()), Ok(h));
        assert_eq!(TokenHash::from_bytes(*h.as_bytes()), h);
        assert!(TokenHash::from_hex("abcd").is_err());
        assert_ne!(TokenHash::of(b"abc"), TokenHash::of(b"abd"));
    }

    #[test]
    fn verdict_distinguishes_live_expired_and_revoked() {
        let live = record();
        assert_eq!(live.verdict(at(1_999)), Ok(()));
        assert_eq!(live.verdict(at(2_000)), Err(ExtractionError::Expired));
        let revoked = IdentityRecord {
            revoked_at: Some(at(1_500)),
            ..record()
        };
        assert_eq!(revoked.verdict(at(1_499)), Ok(()));
        assert_eq!(revoked.verdict(at(1_500)), Err(ExtractionError::Revoked));
        // Revoked and expired: revocation is the operator's act, so it wins.
        assert_eq!(revoked.verdict(at(3_000)), Err(ExtractionError::Revoked));
        let forever = IdentityRecord {
            expires_at: None,
            ..record()
        };
        assert_eq!(forever.verdict(at(i64::from(u32::MAX))), Ok(()));
    }

    #[test]
    fn identity_record_uses_admin_surface_field_names_and_no_token() {
        let json = serde_json::to_value(record()).expect("ser");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "claims",
                "created_at",
                "expires_at",
                "id",
                "identity",
                "label",
                "last_seen_at",
                "revoked_at"
            ]
        );
        assert_eq!(
            serde_json::from_value::<IdentityRecord>(json).expect("de"),
            record()
        );
    }
}
