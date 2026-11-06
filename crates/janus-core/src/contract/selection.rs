//! How an upstream instance picks a credential
//! (`docs/credential-substitution#credential-selection`).
//!
//! Every `[[upstream]]` is an instance of one implementation. Credentials
//! belong to an implementation; a [`Selection`] says which of them one
//! instance may use (its [`CredentialFilter`]) and how it picks among the
//! usable ones when nothing names a credential (its [`Strategy`]). The pool
//! applies it in `CredentialPool::resolve`; the admin surface applies the
//! same filter when it lists an instance's credentials.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::credential::{CredentialKind, CredentialMetadata};
use super::upstream::UpstreamKind;

/// How an instance picks among its usable credentials when the choice is
/// the default chain. Every strategy is deterministic: given the same
/// credentials, the same statuses and the same sequence of requests, it
/// picks the same credentials.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// The first usable credential in chain order.
    #[default]
    Ordered,
    /// Each request takes the next usable credential in chain order,
    /// wrapping around.
    RoundRobin,
    /// The usable credential picked longest ago (or never), ties in chain
    /// order.
    LeastRecentlyUsed,
    /// The usable credential with the highest rendezvous hash of the
    /// verified caller identity and the credential ref; a request with no
    /// verified identity picks as `ordered` does.
    Sticky,
}

impl Strategy {
    pub const ALL: [Strategy; 4] = [
        Strategy::Ordered,
        Strategy::RoundRobin,
        Strategy::LeastRecentlyUsed,
        Strategy::Sticky,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Ordered => "ordered",
            Strategy::RoundRobin => "round_robin",
            Strategy::LeastRecentlyUsed => "least_recently_used",
            Strategy::Sticky => "sticky",
        }
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which of an implementation's credentials an instance may use. Every
/// field that is set must hold (they combine with AND); `tags` holds when
/// the credential carries any one of the listed tags. The empty filter
/// admits every credential of the implementation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<CredentialKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

impl CredentialFilter {
    /// Whether the filter restricts nothing.
    pub fn is_empty(&self) -> bool {
        self.kind.is_none() && self.refs.is_none() && self.tags.is_none()
    }

    /// Whether a credential with this ref, kind and tags passes.
    pub fn admits(&self, credential_ref: &str, kind: CredentialKind, tags: &[String]) -> bool {
        self.kind.is_none_or(|k| k == kind)
            && self
                .refs
                .as_ref()
                .is_none_or(|refs| refs.iter().any(|r| r == credential_ref))
            && self
                .tags
                .as_ref()
                .is_none_or(|want| want.iter().any(|t| tags.contains(t)))
    }
}

/// Why a tag was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidTag {
    #[error("a tag must not be empty")]
    Empty,
    #[error("a tag uses only [A-Za-z0-9._-]; found {0:?}")]
    BadCharacter(char),
}

/// The one tag grammar, shared by config, the pool and the admin surface:
/// non-empty, `[A-Za-z0-9._-]`.
pub fn validate_tag(tag: &str) -> Result<(), InvalidTag> {
    if tag.is_empty() {
        return Err(InvalidTag::Empty);
    }
    match tag
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        Some(c) => Err(InvalidTag::BadCharacter(c)),
        None => Ok(()),
    }
}

/// One upstream instance's selection rules, plus the round-robin cursor
/// they need. Built once from config and shared by the data plane and the
/// admin surface.
#[derive(Debug)]
pub struct Selection {
    instance: String,
    implementation: UpstreamKind,
    filter: CredentialFilter,
    strategy: Strategy,
    failover: bool,
    cursor: AtomicU64,
}

impl Selection {
    pub fn new(
        instance: impl Into<String>,
        implementation: UpstreamKind,
        filter: CredentialFilter,
        strategy: Strategy,
        failover: bool,
    ) -> Self {
        Selection {
            instance: instance.into(),
            implementation,
            filter,
            strategy,
            failover,
            cursor: AtomicU64::new(0),
        }
    }

    /// The selection of an instance named after its implementation with
    /// none of the selection keys set: every credential of the
    /// implementation, `ordered`, no failover.
    pub fn implementation_wide(implementation: UpstreamKind) -> Self {
        Selection::new(
            implementation.as_str(),
            implementation,
            CredentialFilter::default(),
            Strategy::Ordered,
            false,
        )
    }

    /// The instance name: what the request log, `ResolveRoute.upstream` and
    /// `?upstream=` use.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn implementation(&self) -> UpstreamKind {
        self.implementation
    }

    pub fn filter(&self) -> &CredentialFilter {
        &self.filter
    }

    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Whether a default-chain request moves on to the next credential
    /// when the upstream rejects the one it used.
    pub fn failover(&self) -> bool {
        self.failover
    }

    /// Whether this instance may use a credential with this metadata.
    pub fn admits(&self, meta: &CredentialMetadata) -> bool {
        meta.upstream == self.implementation
            && self
                .filter
                .admits(&meta.credential_ref, meta.kind, &meta.tags)
    }

    /// The round-robin position for the next pick, advancing it.
    pub fn advance(&self) -> u64 {
        self.cursor.fetch_add(1, Ordering::Relaxed)
    }

    /// The round-robin position the next pick would use.
    pub fn position(&self) -> u64 {
        self.cursor.load(Ordering::Relaxed)
    }
}

/// What a default-chain pick may take into account beyond the selection:
/// the verified caller identity (for `sticky`) and the credentials already
/// tried for this request (for failover).
#[derive(Debug, Clone, Copy, Default)]
pub struct Pick<'a> {
    pub identity: Option<&'a str>,
    pub exclude: &'a [String],
}

/// The `sticky` score of a credential for an identity: SHA-256 over the
/// identity's length (8 bytes, big-endian), the identity and the ref. The
/// length prefix keeps `("ab", "c")` and `("a", "bc")` apart. The highest
/// score wins, so removing a credential moves only the identities it held.
pub fn sticky_score(identity: &str, credential_ref: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update((identity.len() as u64).to_be_bytes());
    hash.update(identity.as_bytes());
    hash.update(credential_ref.as_bytes());
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_names_round_trip() {
        for s in Strategy::ALL {
            let json = serde_json::to_string(&s).expect("ser");
            assert_eq!(json, format!("\"{s}\""));
            assert_eq!(serde_json::from_str::<Strategy>(&json).expect("de"), s);
        }
        assert_eq!(Strategy::default(), Strategy::Ordered);
    }

    #[test]
    fn filter_fields_combine_with_and_and_tags_with_or() {
        let tags = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let f = CredentialFilter {
            kind: Some(CredentialKind::Oauth),
            refs: None,
            tags: Some(tags(&["team", "personal"])),
        };
        assert!(f.admits("a", CredentialKind::Oauth, &tags(&["personal"])));
        assert!(!f.admits("a", CredentialKind::ApiKey, &tags(&["personal"])));
        assert!(!f.admits("a", CredentialKind::Oauth, &tags(&["other"])));
        assert!(!f.admits("a", CredentialKind::Oauth, &[]));
        let by_ref = CredentialFilter {
            refs: Some(tags(&["a", "b"])),
            ..CredentialFilter::default()
        };
        assert!(by_ref.admits("b", CredentialKind::ApiKey, &[]));
        assert!(!by_ref.admits("c", CredentialKind::ApiKey, &[]));
        assert!(CredentialFilter::default().admits("x", CredentialKind::ApiKey, &[]));
        assert!(CredentialFilter::default().is_empty());
        assert!(!by_ref.is_empty());
    }

    #[test]
    fn tag_grammar() {
        assert_eq!(validate_tag("team-1.a_b"), Ok(()));
        assert_eq!(validate_tag(""), Err(InvalidTag::Empty));
        assert_eq!(validate_tag("a b"), Err(InvalidTag::BadCharacter(' ')));
    }

    #[test]
    fn sticky_score_is_deterministic_and_length_prefixed() {
        assert_eq!(sticky_score("alice", "k1"), sticky_score("alice", "k1"));
        assert_ne!(sticky_score("alice", "k1"), sticky_score("alice", "k2"));
        assert_ne!(sticky_score("ab", "c"), sticky_score("a", "bc"));
    }

    #[test]
    fn round_robin_cursor_advances_and_peeks() {
        let s = Selection::implementation_wide(UpstreamKind::Anthropic);
        assert_eq!(s.instance(), "anthropic");
        assert_eq!(s.position(), 0);
        assert_eq!(s.advance(), 0);
        assert_eq!(s.advance(), 1);
        assert_eq!(s.position(), 2);
        assert!(!s.failover());
        assert_eq!(s.strategy(), Strategy::Ordered);
    }
}
