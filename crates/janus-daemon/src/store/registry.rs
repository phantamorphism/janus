//! The opaque-identity registry (`docs/caller-identity#registry`): the
//! `identities` table, loaded into an in-memory index at open and written
//! through on every mutation.
//!
//! [`Registry::lookup`] — the [`OpaqueRegistry`] probe the `opaque`
//! extractor calls on every request — is one read-locked hash-map probe and
//! an atomic store of `last_seen_at`; it never touches the database.
//! Mutations write the row first and update the index only once the write
//! has committed, so the index never holds a state the file does not, and a
//! revocation is in the index before `revoke` returns: the very next lookup
//! sees it. `last_seen_at` is the one field a lookup changes; it is kept per
//! entry in memory and written out by [`Registry::flush_last_seen`], which
//! the store's background job runs periodically and at shutdown.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use janus_core::contract::registry::{
    IdentityRecord, OpaqueRegistry, RegistryEntry, TokenHash, OPAQUE_TOKEN_BYTES,
};
use rand::Rng;
use rusqlite::{params, Connection};

use super::db::{opt_ts_from_sql, ts_from_sql, ts_to_sql};
use super::StoreError;

/// When a new entry stops verifying: `expires_in` or `expires_at` of
/// `POST /v1/identities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// Relative to the moment of creation.
    In(Duration),
    At(DateTime<Utc>),
}

/// The body of `POST /v1/identities`: everything that *means* something.
/// The token is not here; the daemon generates it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewIdentity {
    pub identity: String,
    pub claims: BTreeMap<String, String>,
    pub label: Option<String>,
    pub expiry: Option<Expiry>,
}

/// The create response: the entry's metadata plus the token, which exists
/// only in this value. Not `Debug`, so it cannot end up in a log line.
pub struct CreatedIdentity {
    pub record: IdentityRecord,
    /// 32 random bytes, base64url without padding.
    pub token: String,
}

/// The filters of `GET /v1/identities`. By default revoked and expired
/// entries are left out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentityFilter {
    pub identity: Option<String>,
    pub include_revoked: bool,
    pub include_expired: bool,
}

/// `last_seen_at` of one entry, shared by every version of that entry's
/// index slot so a lookup racing a revoke is not lost.
#[derive(Debug, Default)]
struct LastSeen {
    /// Nanoseconds since the epoch; 0 = never seen.
    nanos: AtomicI64,
    /// Changed since the last flush.
    dirty: AtomicBool,
}

impl LastSeen {
    fn from(at: Option<DateTime<Utc>>) -> Self {
        LastSeen {
            nanos: AtomicI64::new(at.map_or(0, to_nanos)),
            dirty: AtomicBool::new(false),
        }
    }

    fn get(&self) -> Option<DateTime<Utc>> {
        match self.nanos.load(Ordering::Relaxed) {
            0 => None,
            n => Some(DateTime::from_timestamp_nanos(n)),
        }
    }
}

fn to_nanos(at: DateTime<Utc>) -> i64 {
    // Representable for 1677..2262; clamp rather than panic outside it.
    at.timestamp_nanos_opt()
        .unwrap_or(if at.timestamp() < 0 { 1 } else { i64::MAX })
}

#[derive(Debug, Clone)]
struct Slot {
    /// Authoritative except `last_seen_at`, which lives in `seen`.
    record: IdentityRecord,
    seen: Arc<LastSeen>,
}

impl Slot {
    fn snapshot(&self) -> IdentityRecord {
        IdentityRecord {
            last_seen_at: self.seen.get(),
            ..self.record.clone()
        }
    }
}

#[derive(Debug, Default)]
struct Index {
    by_hash: HashMap<TokenHash, Slot>,
    hash_of_id: HashMap<String, TokenHash>,
}

impl Index {
    fn insert(&mut self, hash: TokenHash, slot: Slot) {
        self.hash_of_id.insert(slot.record.id.clone(), hash);
        self.by_hash.insert(hash, slot);
    }

    fn slot(&self, id: &str) -> Option<&Slot> {
        self.hash_of_id.get(id).and_then(|h| self.by_hash.get(h))
    }
}

/// The registry. Cheap to share behind an `Arc`; every method takes `&self`.
#[derive(Debug)]
pub struct Registry {
    index: RwLock<Index>,
    /// Write-through connection; also serializes mutations, so the index
    /// and the table change in the same order.
    conn: Mutex<Connection>,
}

impl Registry {
    /// Load every row of `identities` into the index.
    pub(super) fn load(conn: Connection) -> Result<Self, StoreError> {
        let mut index = Index::default();
        for entry in read_all(&conn)? {
            let seen = Arc::new(LastSeen::from(entry.record.last_seen_at));
            index.insert(
                entry.token_hash,
                Slot {
                    record: entry.record,
                    seen,
                },
            );
        }
        Ok(Registry {
            index: RwLock::new(index),
            conn: Mutex::new(conn),
        })
    }

    /// Register a new entry with a freshly generated token. The token is
    /// returned here once; only its hash is kept.
    pub fn create(
        &self,
        new: NewIdentity,
        now: DateTime<Utc>,
    ) -> Result<CreatedIdentity, StoreError> {
        if new.identity.is_empty() {
            return Err(StoreError::Invalid("identity must not be empty".into()));
        }
        let expires_at = match new.expiry {
            None => None,
            Some(Expiry::At(at)) => Some(at),
            Some(Expiry::In(d)) => Some(
                chrono::Duration::from_std(d)
                    .ok()
                    .and_then(|d| now.checked_add_signed(d))
                    .ok_or_else(|| StoreError::Invalid("expires_in is out of range".into()))?,
            ),
        };
        if expires_at.is_some_and(|at| at <= now) {
            return Err(StoreError::Invalid("expiry must be in the future".into()));
        }
        let mut bytes = [0u8; OPAQUE_TOKEN_BYTES];
        rand::rng().fill_bytes(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let token_hash = TokenHash::of(token.as_bytes());
        let record = IdentityRecord {
            id: uuid::Uuid::now_v7().to_string(),
            identity: new.identity,
            claims: new.claims,
            label: new.label,
            created_at: now,
            expires_at,
            revoked_at: None,
            last_seen_at: None,
        };
        let conn = self.lock_conn();
        let claims = serde_json::to_string(&record.claims)
            .map_err(|e| StoreError::Invalid(format!("claims do not serialize: {e}")))?;
        conn.execute(
            "INSERT INTO identities
               (id, token_hash, identity, claims, label, created_at, expires_at, revoked_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL)",
            params![
                record.id,
                token_hash.to_hex(),
                record.identity,
                claims,
                record.label,
                ts_to_sql(record.created_at),
                record.expires_at.map(ts_to_sql),
            ],
        )?;
        self.write_index().insert(
            token_hash,
            Slot {
                record: record.clone(),
                seen: Arc::new(LastSeen::default()),
            },
        );
        drop(conn);
        Ok(CreatedIdentity { record, token })
    }

    /// One entry by `id`.
    pub fn get(&self, id: &str) -> Option<IdentityRecord> {
        self.read_index().slot(id).map(Slot::snapshot)
    }

    /// Entries matching `filter` at `now`, newest first.
    pub fn list(&self, filter: &IdentityFilter, now: DateTime<Utc>) -> Vec<IdentityRecord> {
        let mut out: Vec<IdentityRecord> = self
            .read_index()
            .by_hash
            .values()
            .map(Slot::snapshot)
            .filter(|r| filter.identity.as_ref().is_none_or(|i| *i == r.identity))
            .filter(|r| filter.include_revoked || r.revoked_at.is_none_or(|at| at > now))
            .filter(|r| filter.include_expired || r.expires_at.is_none_or(|at| at > now))
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        out
    }

    /// Revoke an entry, effective for the next lookup. Idempotent: an
    /// already-revoked entry keeps its original `revoked_at`.
    pub fn revoke(&self, id: &str, now: DateTime<Utc>) -> Result<IdentityRecord, StoreError> {
        let conn = self.lock_conn();
        let (hash, slot) = {
            let index = self.read_index();
            let hash = *index
                .hash_of_id
                .get(id)
                .ok_or_else(|| StoreError::NotFound(id.to_owned()))?;
            (hash, index.by_hash[&hash].clone())
        };
        if slot.record.revoked_at.is_some() {
            return Ok(slot.snapshot());
        }
        conn.execute(
            "UPDATE identities SET revoked_at = ?2 WHERE id = ?1",
            params![id, ts_to_sql(now)],
        )?;
        let revoked = Slot {
            record: IdentityRecord {
                revoked_at: Some(now),
                ..slot.record
            },
            seen: slot.seen,
        };
        let snapshot = revoked.snapshot();
        self.write_index().insert(hash, revoked);
        Ok(snapshot)
    }

    /// Remove an entry outright.
    pub fn delete(&self, id: &str) -> Result<(), StoreError> {
        let conn = self.lock_conn();
        let removed = conn.execute("DELETE FROM identities WHERE id = ?1", params![id])?;
        let mut index = self.write_index();
        let in_index = index
            .hash_of_id
            .remove(id)
            .and_then(|h| index.by_hash.remove(&h))
            .is_some();
        if removed == 0 && !in_index {
            return Err(StoreError::NotFound(id.to_owned()));
        }
        Ok(())
    }

    /// Write every `last_seen_at` changed since the previous flush. Returns
    /// how many entries were written.
    pub fn flush_last_seen(&self) -> Result<usize, StoreError> {
        let dirty: Vec<(String, Arc<LastSeen>)> = self
            .read_index()
            .by_hash
            .values()
            .filter(|s| s.seen.dirty.swap(false, Ordering::AcqRel))
            .map(|s| (s.record.id.clone(), Arc::clone(&s.seen)))
            .collect();
        if dirty.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock_conn();
        let result = (|| {
            let tx = conn.transaction()?;
            {
                let mut stmt =
                    tx.prepare("UPDATE identities SET last_seen_at = ?2 WHERE id = ?1")?;
                for (id, seen) in &dirty {
                    stmt.execute(params![id, seen.get().map(ts_to_sql)])?;
                }
            }
            tx.commit()
        })();
        if let Err(e) = result {
            // Nothing was written; try again on the next flush.
            for (_, seen) in &dirty {
                seen.dirty.store(true, Ordering::Release);
            }
            return Err(e.into());
        }
        Ok(dirty.len())
    }

    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn read_index(&self) -> std::sync::RwLockReadGuard<'_, Index> {
        self.index.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write_index(&self) -> std::sync::RwLockWriteGuard<'_, Index> {
        self.index.write().unwrap_or_else(|p| p.into_inner())
    }
}

impl OpaqueRegistry for Registry {
    fn lookup(&self, token_hash: &TokenHash, now: DateTime<Utc>) -> Option<IdentityRecord> {
        let index = self.read_index();
        let slot = index.by_hash.get(token_hash)?;
        let prev = slot.seen.nanos.fetch_max(to_nanos(now), Ordering::Relaxed);
        if prev < to_nanos(now) {
            slot.seen.dirty.store(true, Ordering::Release);
        }
        Some(slot.snapshot())
    }
}

fn read_all(conn: &Connection) -> Result<Vec<RegistryEntry>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT token_hash, id, identity, claims, label, created_at, expires_at, revoked_at, last_seen_at
         FROM identities",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<String>>(8)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (hash, id, identity, claims, label, created, expires, revoked, seen) = row?;
        let token_hash = TokenHash::from_hex(&hash)
            .map_err(|e| StoreError::Corrupt(format!("identity {id}: bad token_hash: {e}")))?;
        let claims = serde_json::from_str(&claims)
            .map_err(|e| StoreError::Corrupt(format!("identity {id}: bad claims: {e}")))?;
        out.push(RegistryEntry {
            token_hash,
            record: IdentityRecord {
                identity,
                claims,
                label,
                created_at: ts_from_sql(&created)?,
                expires_at: opt_ts_from_sql(expires)?,
                revoked_at: opt_ts_from_sql(revoked)?,
                last_seen_at: opt_ts_from_sql(seen)?,
                id,
            },
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::{at, open_store};
    use janus_core::contract::identity::ExtractionError;
    use rusqlite::OptionalExtension;

    /// Whether a row with this id exists on disk (read from the file
    /// directly to prove write-through).
    fn row_exists(conn: &Connection, id: &str) -> bool {
        conn.query_row("SELECT 1 FROM identities WHERE id = ?1", [id], |_| Ok(()))
            .optional()
            .expect("query")
            .is_some()
    }

    fn new(identity: &str) -> NewIdentity {
        NewIdentity {
            identity: identity.into(),
            claims: BTreeMap::from([("tenant".to_owned(), "acme".to_owned())]),
            label: Some("sandbox".into()),
            expiry: None,
        }
    }

    fn raw(dir: &tempfile::TempDir) -> Connection {
        Connection::open(dir.path().join("janus.db")).expect("raw open")
    }

    #[test]
    fn registry_token_stored_hashed() {
        let (dir, store) = open_store();
        let created = store
            .registry()
            .create(new("sandbox-7"), at(1_000))
            .expect("create");
        // 32 random bytes, base64url without padding.
        assert_eq!(created.token.len(), 43);
        let decoded = URL_SAFE_NO_PAD.decode(&created.token).expect("base64url");
        assert_eq!(decoded.len(), OPAQUE_TOKEN_BYTES);

        let conn = raw(&dir);
        let (hash, claims): (String, String) = conn
            .query_row(
                "SELECT token_hash, claims FROM identities WHERE id = ?1",
                [&created.record.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row");
        assert_eq!(hash, TokenHash::of(created.token.as_bytes()).to_hex());
        assert_eq!(claims, r#"{"tenant":"acme"}"#);
        // The token appears nowhere in the file, in any column.
        drop(store);
        let bytes = std::fs::read(dir.path().join("janus.db")).expect("read db");
        let wal = std::fs::read(dir.path().join("janus.db-wal")).unwrap_or_default();
        for haystack in [&bytes, &wal] {
            assert!(
                !haystack
                    .windows(created.token.len())
                    .any(|w| w == created.token.as_bytes()),
                "token found in database file"
            );
            assert!(
                !haystack.windows(32).any(|w| w == decoded.as_slice()),
                "raw token bytes found in database file"
            );
        }
    }

    #[test]
    fn token_returned_once() {
        let (_dir, store) = open_store();
        let reg = store.registry();
        let a = reg.create(new("sandbox-7"), at(1_000)).expect("create");
        let b = reg.create(new("sandbox-7"), at(1_000)).expect("create");
        assert_ne!(a.token, b.token, "every create generates a fresh token");
        assert_ne!(a.record.id, b.record.id);
        // Nothing that reads an entry back carries the token or its hash.
        for record in [reg.get(&a.record.id).expect("get")]
            .into_iter()
            .chain(reg.list(&IdentityFilter::default(), at(1_001)))
        {
            let json = serde_json::to_string(&record).expect("ser");
            assert!(
                !json.contains(&a.token) && !json.contains(&b.token),
                "{json}"
            );
            assert!(!json.contains(&TokenHash::of(a.token.as_bytes()).to_hex()));
        }
        // The token is only good for lookup, and lookup is by hash.
        let found = reg
            .lookup(&TokenHash::of(a.token.as_bytes()), at(1_001))
            .expect("hit");
        assert_eq!(found.id, a.record.id);
        assert!(reg.lookup(&TokenHash::of(b"guess"), at(1_001)).is_none());
    }

    #[test]
    fn revoked_token_rejected_immediately() {
        let (_dir, store) = open_store();
        let reg = store.registry();
        let c = reg.create(new("sandbox-7"), at(1_000)).expect("create");
        let hash = TokenHash::of(c.token.as_bytes());
        let live = reg.lookup(&hash, at(1_001)).expect("hit");
        assert_eq!(live.verdict(at(1_001)), Ok(()));

        let revoked = reg.revoke(&c.record.id, at(1_002)).expect("revoke");
        assert_eq!(revoked.revoked_at, Some(at(1_002)));
        // The very next lookup, with no flush or reload in between.
        let next = reg.lookup(&hash, at(1_002)).expect("still present");
        assert_eq!(next.verdict(at(1_002)), Err(ExtractionError::Revoked));

        // Idempotent: a second revoke keeps the first timestamp.
        let again = reg.revoke(&c.record.id, at(5_000)).expect("revoke again");
        assert_eq!(again.revoked_at, Some(at(1_002)));
        assert!(matches!(
            reg.revoke("no-such-id", at(5_000)),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn revocation_and_entries_persist_across_reopen() {
        let (dir, store) = open_store();
        let reg = store.registry();
        let keep = reg.create(new("sandbox-7"), at(1_000)).expect("create");
        let gone = reg.create(new("sandbox-8"), at(1_000)).expect("create");
        reg.revoke(&keep.record.id, at(1_500)).expect("revoke");
        reg.delete(&gone.record.id).expect("delete");
        assert!(
            !row_exists(&raw(&dir), &gone.record.id),
            "delete writes through"
        );
        drop(store);

        let store = crate::store::test_support::reopen(&dir);
        let reg = store.registry();
        let back = reg
            .lookup(&TokenHash::of(keep.token.as_bytes()), at(2_000))
            .expect("loaded from disk");
        assert_eq!(back.identity, "sandbox-7");
        assert_eq!(back.claims, keep.record.claims);
        assert_eq!(back.label.as_deref(), Some("sandbox"));
        assert_eq!(back.created_at, at(1_000));
        assert_eq!(back.revoked_at, Some(at(1_500)));
        assert_eq!(back.verdict(at(2_000)), Err(ExtractionError::Revoked));
        assert!(reg
            .lookup(&TokenHash::of(gone.token.as_bytes()), at(2_000))
            .is_none());
        assert!(reg.get(&gone.record.id).is_none());
        assert!(matches!(
            reg.delete(&gone.record.id),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn expiry_fails_verification_and_list_filters_it() {
        let (_dir, store) = open_store();
        let reg = store.registry();
        let short = reg
            .create(
                NewIdentity {
                    expiry: Some(Expiry::In(Duration::from_secs(60))),
                    ..new("sandbox-7")
                },
                at(1_000),
            )
            .expect("create");
        assert_eq!(short.record.expires_at, Some(at(1_060)));
        let pinned = reg
            .create(
                NewIdentity {
                    expiry: Some(Expiry::At(at(5_000))),
                    ..new("sandbox-7")
                },
                at(1_000),
            )
            .expect("create");
        let other = reg.create(new("sandbox-9"), at(1_001)).expect("create");
        let revoked = reg.create(new("sandbox-7"), at(1_002)).expect("create");
        reg.revoke(&revoked.record.id, at(1_003)).expect("revoke");

        let hit = reg
            .lookup(&TokenHash::of(short.token.as_bytes()), at(1_060))
            .expect("expired entries are still found");
        assert_eq!(hit.verdict(at(1_060)), Err(ExtractionError::Expired));

        let ids = |f: IdentityFilter| -> Vec<String> {
            reg.list(&f, at(2_000)).into_iter().map(|r| r.id).collect()
        };
        // Default: live entries only, newest first.
        assert_eq!(
            ids(IdentityFilter::default()),
            [other.record.id.clone(), pinned.record.id.clone()]
        );
        assert_eq!(
            ids(IdentityFilter {
                include_expired: true,
                identity: Some("sandbox-7".into()),
                ..Default::default()
            }),
            [pinned.record.id.clone(), short.record.id.clone()]
        );
        assert_eq!(
            ids(IdentityFilter {
                include_revoked: true,
                include_expired: true,
                identity: Some("sandbox-7".into()),
            })
            .len(),
            3
        );

        for bad in [
            NewIdentity {
                expiry: Some(Expiry::At(at(1_000))),
                ..new("x")
            },
            NewIdentity {
                expiry: Some(Expiry::In(Duration::from_secs(u64::MAX))),
                ..new("x")
            },
            new(""),
        ] {
            assert!(matches!(
                reg.create(bad, at(1_000)),
                Err(StoreError::Invalid(_))
            ));
        }
    }

    #[test]
    fn last_seen_is_in_memory_until_flushed() {
        let (dir, store) = open_store();
        let reg = store.registry();
        let c = reg.create(new("sandbox-7"), at(1_000)).expect("create");
        let hash = TokenHash::of(c.token.as_bytes());
        assert_eq!(
            reg.lookup(&hash, at(1_010)).expect("hit").last_seen_at,
            Some(at(1_010))
        );
        // An out-of-order (older) lookup never moves it backwards.
        reg.lookup(&hash, at(1_005));
        assert_eq!(
            reg.get(&c.record.id).expect("get").last_seen_at,
            Some(at(1_010))
        );

        let on_disk = |conn: &Connection| -> Option<String> {
            conn.query_row(
                "SELECT last_seen_at FROM identities WHERE id = ?1",
                [&c.record.id],
                |r| r.get(0),
            )
            .expect("row")
        };
        let conn = raw(&dir);
        assert_eq!(on_disk(&conn), None, "a lookup is not a write");
        assert_eq!(reg.flush_last_seen().expect("flush"), 1);
        assert_eq!(on_disk(&conn), Some(ts_to_sql(at(1_010))));
        assert_eq!(
            reg.flush_last_seen().expect("flush"),
            0,
            "nothing new to write"
        );

        // A revoke between lookup and flush does not lose the lookup.
        reg.lookup(&hash, at(1_020));
        reg.revoke(&c.record.id, at(1_021)).expect("revoke");
        assert_eq!(reg.flush_last_seen().expect("flush"), 1);
        assert_eq!(on_disk(&conn), Some(ts_to_sql(at(1_020))));
        drop(store);
        let store = crate::store::test_support::reopen(&dir);
        assert_eq!(
            store
                .registry()
                .get(&c.record.id)
                .expect("get")
                .last_seen_at,
            Some(at(1_020))
        );
    }

    #[test]
    fn a_corrupt_row_refuses_to_load() {
        let (dir, store) = open_store();
        store
            .registry()
            .create(new("sandbox-7"), at(1_000))
            .expect("create");
        drop(store);
        raw(&dir)
            .execute("UPDATE identities SET token_hash = 'zz'", [])
            .expect("corrupt");
        match crate::store::Store::open(dir.path(), &Default::default()) {
            Err(StoreError::Corrupt(msg)) => assert!(msg.contains("token_hash"), "{msg}"),
            Err(other) => panic!("expected Corrupt, got {other:?}"),
            Ok(_) => panic!("expected Corrupt, got a store"),
        }
    }
}
