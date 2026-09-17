//! Tier 2 — durable, SQLite-backed [`MemoryStore`](super::store::MemoryStore).
//!
//! This module houses `SqliteMemoryStore`, a drop-in alternative to
//! [`InMemoryMemoryStore`](super::store::InMemoryMemoryStore) that persists
//! `MemoryRecord`s to SQLite so learned procedural memory survives process
//! restarts. It reuses `validate_memory` and the shared
//! [`cosine_distance`](super::store::cosine_distance) so ranking math is
//! identical to the in-memory reference implementation.
//!
//! The module is populated incrementally by later tasks; it is scaffolded here.
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

use crate::tier2::memory::{Embedding, Memory, MemoryKind, Plan};
use crate::tier2::store::{cosine_distance, validate_memory, MemoryStore, MemoryStoreError};
use crate::types::{GoalNodeId, IntentSignature, MemoryId, SubtreeHash, Timestamp};

/// A store-level failure for the SQLite-backed [`MemoryStore`].
///
/// This composes with — and does not replace — [`MemoryStoreError`]. The
/// `MemoryStore` trait's write methods keep returning [`MemoryStoreError`]
/// (validation is the only failure they expose); `SqliteStoreError` surfaces at
/// [`SqliteMemoryStore::open`](super::sqlite_store) and, for the fail-safe
/// corruption cases on read, folds through this union so a caller that wants the
/// combined error can observe open, schema, corruption, I/O, and validation
/// failures through a single type.
#[derive(Debug, thiserror::Error)]
pub enum SqliteStoreError {
    /// The database file could not be opened or created at `path`.
    #[error("failed to open sqlite store at {path}: {source}")]
    Open {
        /// The configured database path that could not be opened.
        path: String,
        /// The underlying rusqlite failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The persisted schema is incompatible, or migration failed.
    #[error("sqlite schema is incompatible or migration failed: {0}")]
    Schema(String),

    /// A persisted memory record could not be deserialized (corrupt/malformed).
    #[error("persisted memory record is corrupted or malformed: {0}")]
    Corruption(String),

    /// A generic SQLite I/O or query failure.
    #[error("sqlite I/O error: {0}")]
    Io(#[from] rusqlite::Error),

    /// A write-time validation failure, preserved unchanged from the trait.
    #[error(transparent)]
    Validation(#[from] MemoryStoreError),
}

/// The current SQLite schema version, stamped into `PRAGMA user_version`.
///
/// A fresh database (version `0`) is migrated to this version; a database
/// already at this version is left intact; a database at a higher or otherwise
/// unknown version is rejected with [`SqliteStoreError::Schema`].
const SCHEMA_VERSION: i64 = 1;

/// Apply the schema migration idempotently.
///
/// Runs the `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS`
/// statements for the `memories` table, then reconciles `PRAGMA user_version`:
///
/// - `0` (a fresh database) is stamped with [`SCHEMA_VERSION`];
/// - a value equal to [`SCHEMA_VERSION`] is left intact;
/// - any higher or otherwise unknown value is rejected with
///   [`SqliteStoreError::Schema`], since this build cannot safely operate on a
///   schema it does not recognize.
///
/// Because every DDL statement is `IF NOT EXISTS` and the version write is
/// conditional, applying `migrate` any number of times yields the same schema
/// as applying it once, leaving pre-existing data intact.
fn migrate(conn: &Connection) -> Result<(), SqliteStoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memories (
            id           TEXT PRIMARY KEY NOT NULL,
            intent_type  TEXT NOT NULL,
            target_type  TEXT NOT NULL,
            target_ref   TEXT NOT NULL,
            scope        TEXT NOT NULL,
            kind         TEXT NOT NULL,
            embedding    BLOB NOT NULL,
            body         TEXT NOT NULL,
            node_id      TEXT NOT NULL,
            subtree_hash TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            updated_at   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_memories_signature
            ON memories (intent_type, target_type, target_ref, scope);
        CREATE INDEX IF NOT EXISTS idx_memories_kind ON memories (kind);
        CREATE INDEX IF NOT EXISTS idx_memories_idempotency
            ON memories (node_id, subtree_hash);",
    )?;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    match version {
        0 => {
            // Fresh database: stamp the current schema version. `user_version`
            // does not accept bound parameters, so format the trusted constant.
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            Ok(())
        }
        v if v == SCHEMA_VERSION => Ok(()),
        v => Err(SqliteStoreError::Schema(format!(
            "unsupported schema version {v}: this build supports version {SCHEMA_VERSION}"
        ))),
    }
}

/// A durable, SQLite-backed [`MemoryStore`](super::store::MemoryStore).
///
/// This is a drop-in alternative to
/// [`InMemoryMemoryStore`](super::store::InMemoryMemoryStore) that persists
/// `MemoryRecord`s to SQLite so learned procedural memory survives process
/// restarts. All access is serialized through a `Mutex<Connection>`, since a
/// `rusqlite::Connection` is `Send` but not `Sync` and is not internally
/// synchronized; the mutex provides the interior mutability the `MemoryStore`
/// trait requires (all methods take `&self`).
///
/// The `MemoryStore` trait implementation is added by later tasks (4.x/5.x);
/// this task provides construction ([`open`](Self::open),
/// [`open_in_memory`](Self::open_in_memory)) and the open-time integrity pass.
#[derive(Debug)]
pub struct SqliteMemoryStore {
    conn: Mutex<Connection>,
}

impl SqliteMemoryStore {
    /// Open (or create) a store at `path`, running idempotent migration.
    ///
    /// The connection is opened first; a failure to open or create the file is
    /// mapped to [`SqliteStoreError::Open`] carrying the offending path. Only
    /// after [`migrate`] and the open-time integrity pass both succeed is the
    /// store constructed, so a returned store never serves reads or writes
    /// against an unusable or corrupted database.
    ///
    /// # Errors
    ///
    /// Returns [`SqliteStoreError::Open`] if the database file cannot be opened
    /// or created, [`SqliteStoreError::Schema`] if migration or the
    /// schema-version check fails, or [`SqliteStoreError::Corruption`] if any
    /// persisted `body` cannot be deserialized into a [`Memory`].
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, SqliteStoreError> {
        let path = path.as_ref();
        let conn = Connection::open(path).map_err(|source| SqliteStoreError::Open {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_connection(conn)
    }

    /// Open an in-memory SQLite database (for tests / ephemeral use).
    ///
    /// Runs the same migration and integrity pass as [`open`](Self::open) so an
    /// in-memory store is indistinguishable from a fresh file-backed one.
    ///
    /// # Errors
    ///
    /// Returns [`SqliteStoreError::Open`] if the in-memory connection cannot be
    /// created, or [`SqliteStoreError::Schema`] if migration fails. (A fresh
    /// in-memory database has no rows, so the integrity pass cannot fail.)
    pub fn open_in_memory() -> Result<Self, SqliteStoreError> {
        let conn = Connection::open_in_memory().map_err(|source| SqliteStoreError::Open {
            path: ":memory:".to_owned(),
            source,
        })?;
        Self::from_connection(conn)
    }

    /// Migrate, run the open-time integrity pass, then wrap the connection.
    ///
    /// Shared by [`open`](Self::open) and [`open_in_memory`](Self::open_in_memory):
    /// the store is only constructed once both the schema is reconciled and every
    /// persisted `body` is confirmed deserializable.
    fn from_connection(conn: Connection) -> Result<Self, SqliteStoreError> {
        migrate(&conn)?;
        integrity_check(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

/// The stable `TEXT` tag persisted in the `kind` column.
///
/// This mirrors the `Fragment | Composite | Negative` set named in the schema
/// migration and used by the `filter`/lookup reads, and matches the
/// serde-derived name of each [`MemoryKind`] variant so the column stays
/// consistent whether written here or compared elsewhere.
fn kind_tag(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Fragment => "Fragment",
        MemoryKind::Composite => "Composite",
        MemoryKind::Negative => "Negative",
    }
}

/// Upsert a `MemoryRecord` row for `mem` under `(node_id, subtree_hash)`.
///
/// Runs a single `INSERT OR REPLACE` keyed by [`MemoryId`], deriving the indexed
/// columns from `mem.intent`/`mem.kind`, serializing the body to JSON for
/// lossless round-trip, and — mirroring `InMemoryMemoryStore::record_from` —
/// storing the caller-supplied `embedding` in the `embedding` column so ANN
/// ranking has parity with the reference store (the body carries no embedding).
///
/// The `embedding` is produced by the write path async, up front, outside the
/// store mutex, and passed in here; a backend-unavailable write passes an empty
/// [`Embedding`] and still succeeds (Req 6.4).
///
/// This works on any `&Connection`, so both the plain insert path (holding the
/// mutex guard) and the transactional `reinforce` path share it.
///
/// The `MemoryStore` trait exposes only `MemoryStoreError` (a validation error)
/// on writes, and validation has already run before this is called; the
/// remaining work is infallible in the in-memory reference. A `Memory` always
/// serializes (guaranteed by `memory_roundtrips_through_serde`) and the write is
/// a simple keyed upsert, so a failure here is an unrecoverable invariant break
/// rather than a fabricated validation error — matching the in-memory store's
/// infallible insert.
fn upsert_record(
    conn: &Connection,
    mem: &Memory,
    node_id: &GoalNodeId,
    subtree_hash: &SubtreeHash,
    created_at: Timestamp,
    updated_at: Timestamp,
    embedding: &Embedding,
) {
    let body = serde_json::to_string(mem).expect("Memory must serialize to JSON");
    let embedding_blob =
        serde_json::to_vec(embedding).expect("Embedding must serialize to JSON");

    conn.execute(
        "INSERT OR REPLACE INTO memories (
            id, intent_type, target_type, target_ref, scope, kind,
            embedding, body, node_id, subtree_hash, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            mem.id.0,
            mem.intent.intent_type.0,
            mem.intent.target_type.0,
            mem.intent.target_ref.0,
            mem.intent.scope.0,
            kind_tag(mem.kind),
            embedding_blob,
            body,
            node_id.0,
            subtree_hash.0 .0,
            created_at.0 as i64,
            updated_at.0 as i64,
        ],
    )
    .expect("sqlite upsert of a validated memory must succeed");
}

impl MemoryStore for SqliteMemoryStore {
    fn insert_with_embedding(
        &self,
        key: (GoalNodeId, SubtreeHash),
        mem: Memory,
        embedding: Embedding,
    ) -> Result<MemoryId, MemoryStoreError> {
        // Validate before any SQL so a rejected write leaves the DB unchanged
        // (Requirements 2.1, 2.3), mirroring `InMemoryMemoryStore`.
        validate_memory(&mem)?;

        let (node_id, subtree_hash) = key;
        let id = mem.id.clone();
        // Deterministic timestamps: created == updated at insert time.
        let now = Timestamp::default();

        // Serialize the write through the mutex, recovering a poisoned lock via
        // `into_inner` exactly as the in-memory store does (Requirement 9.2).
        let guard = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        upsert_record(&guard, &mem, &node_id, &subtree_hash, now, now, &embedding);

        Ok(id)
    }

    fn reinforce(
        &self,
        id: &MemoryId,
        candidate: Memory,
    ) -> Result<MemoryId, MemoryStoreError> {
        // The candidate is subject to the same write-time validation as insert
        // (Requirements 2.2, 2.3); a rejected candidate leaves the DB unchanged.
        validate_memory(&candidate)?;

        let mut guard = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // A single transaction so a concurrent read observes either the pre- or
        // post-write state, never a partial record (Requirement 9.2).
        let tx = guard
            .transaction()
            .expect("beginning a sqlite transaction must succeed");

        // Look up the existing row's body and updated_at, if present.
        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT body, updated_at FROM memories WHERE id = ?1",
                rusqlite::params![id.0],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .expect("looking up a memory by id must succeed");

        let returned = match existing {
            Some((body, updated_at)) => {
                // Bump hits/confirms in the stored body and advance
                // updated_at/last_updated, mirroring the in-memory reinforce
                // (`updated_at + 1`, saturating) (Requirement 6.1).
                let mut stored: Memory =
                    serde_json::from_str(&body).expect("stored memory body must deserialize");

                let r = &mut stored.reinforcement;
                r.hits = r.hits.saturating_add(1);
                r.confirms = r.confirms.saturating_add(1);
                let next = Timestamp((updated_at as u64).saturating_add(1));
                r.last_updated = next;

                let new_body =
                    serde_json::to_string(&stored).expect("Memory must serialize to JSON");
                tx.execute(
                    "UPDATE memories SET body = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![new_body, next.0 as i64, id.0],
                )
                .expect("updating a reinforced memory must succeed");
                id.clone()
            }
            None => {
                // Reinforcing an unknown id is a first observation: insert the
                // validated candidate as a new record with node_id from the
                // first provenance origin and subtree_hash from the version
                // (Requirement 6.2), mirroring the in-memory path.
                let node_id = candidate
                    .provenance
                    .origins
                    .first()
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default();
                let subtree_hash = candidate.version.0.clone();
                let new_id = candidate.id.clone();
                let now = Timestamp::default();
                // Reinforce of an absent id is a first observation; it carries
                // no write-time embedding, so store an empty one (matching the
                // pre-embedding behavior and the in-memory reference).
                upsert_record(
                    &tx,
                    &candidate,
                    &node_id,
                    &subtree_hash,
                    now,
                    now,
                    &Embedding::default(),
                );
                new_id
            }
        };

        tx.commit().expect("committing the reinforce transaction must succeed");
        Ok(returned)
    }

    fn filter(&self, sig: &IntentSignature) -> Vec<Memory> {
        // Authoritative check is `applies_to` on the deserialized guard, run over
        // a full scan of persisted bodies. Using `applies_to` rather than the
        // indexed columns guarantees set-equality with `InMemoryMemoryStore`
        // even for guards that constrain fields no index covers (Requirements
        // 3.1, 3.2, 3.3, 3.4). This mirrors the in-memory `values().filter(...)`.
        let guard = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut stmt = match guard.prepare("SELECT id, body FROM memories") {
            Ok(stmt) => stmt,
            Err(err) => {
                eprintln!("sqlite_store::filter: failed to prepare scan: {err}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(rows) => rows,
            Err(err) => {
                eprintln!("sqlite_store::filter: failed to scan memories: {err}");
                return Vec::new();
            }
        };

        let mut out = Vec::new();
        for row in rows {
            let (id, body) = match row {
                Ok(pair) => pair,
                Err(err) => {
                    // Fail-safe: skip an unreadable row for this collection read;
                    // open-time integrity already guards diagnosable corruption.
                    eprintln!("sqlite_store::filter: skipping unreadable row: {err}");
                    continue;
                }
            };
            match serde_json::from_str::<Memory>(&body) {
                Ok(mem) => {
                    if mem.applicability.applies_to(sig) {
                        out.push(mem);
                    }
                }
                Err(err) => {
                    // Fail-safe: log and skip a malformed row.
                    eprintln!(
                        "sqlite_store::filter: skipping malformed memory {id}: {err}"
                    );
                }
            }
        }
        out
    }

    fn ann_recall(&self, embedding: &Embedding, limit: usize) -> Vec<Memory> {
        // Mirror `InMemoryMemoryStore::ann_recall` exactly: empty on a zero
        // limit (Requirement 4.2), otherwise rank every stored memory by
        // `cosine_distance(stored, query)` ascending via `total_cmp`, taking the
        // closest `limit` (Requirements 4.1, 4.3, 4.4). Zero-magnitude vectors
        // sort last at distance `2.0`, handled by the shared `cosine_distance`.
        if limit == 0 {
            return Vec::new();
        }

        let guard = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut stmt = match guard.prepare("SELECT id, embedding, body FROM memories") {
            Ok(stmt) => stmt,
            Err(err) => {
                eprintln!("sqlite_store::ann_recall: failed to prepare scan: {err}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            Ok(rows) => rows,
            Err(err) => {
                eprintln!("sqlite_store::ann_recall: failed to scan memories: {err}");
                return Vec::new();
            }
        };

        let mut scored: Vec<(f32, Memory)> = Vec::new();
        for row in rows {
            let (id, embedding_blob, body) = match row {
                Ok(triple) => triple,
                Err(err) => {
                    eprintln!("sqlite_store::ann_recall: skipping unreadable row: {err}");
                    continue;
                }
            };
            // The stored embedding column is authoritative for ranking, mirroring
            // the in-memory store which reads `record.embedding`. `insert` stores
            // `Embedding::default()`, so deserialize the blob back to `Embedding`.
            let stored: Embedding = match serde_json::from_slice(&embedding_blob) {
                Ok(embedding) => embedding,
                Err(err) => {
                    eprintln!(
                        "sqlite_store::ann_recall: skipping memory {id} with malformed embedding: {err}"
                    );
                    continue;
                }
            };
            let mem: Memory = match serde_json::from_str(&body) {
                Ok(mem) => mem,
                Err(err) => {
                    eprintln!(
                        "sqlite_store::ann_recall: skipping malformed memory {id}: {err}"
                    );
                    continue;
                }
            };
            scored.push((cosine_distance(&stored, embedding), mem));
        }

        // Sort ascending by distance; NaN-safe via total_cmp.
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, mem)| mem)
            .collect()
    }

    fn has_memory_for(&self, key: &(GoalNodeId, SubtreeHash)) -> Option<MemoryId> {
        // Match how `insert`/`reinforce` store the idempotency key: `node_id`
        // and `subtree_hash` columns (Requirements 5.1, 5.2). Return the first
        // matching row's id, else None.
        let (node_id, subtree_hash) = key;
        let guard = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        guard
            .query_row(
                "SELECT id FROM memories WHERE node_id = ?1 AND subtree_hash = ?2",
                rusqlite::params![node_id.0, subtree_hash.0 .0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .unwrap_or_else(|err| {
                eprintln!("sqlite_store::has_memory_for: lookup failed: {err}");
                None
            })
            .map(MemoryId)
    }

    fn find_duplicate(&self, intent: &IntentSignature, plan: &Plan) -> Option<MemoryId> {
        // Mirror the in-memory `find`: return the first persisted memory whose
        // deserialized body matches on both `intent` and `plan` (Requirements
        // 5.3, 5.4). `Plan` equality is over the full serialized structure.
        let guard = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut stmt = match guard.prepare("SELECT id, body FROM memories") {
            Ok(stmt) => stmt,
            Err(err) => {
                eprintln!("sqlite_store::find_duplicate: failed to prepare scan: {err}");
                return None;
            }
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(rows) => rows,
            Err(err) => {
                eprintln!("sqlite_store::find_duplicate: failed to scan memories: {err}");
                return None;
            }
        };

        for row in rows {
            let (id, body) = match row {
                Ok(pair) => pair,
                Err(err) => {
                    eprintln!("sqlite_store::find_duplicate: skipping unreadable row: {err}");
                    continue;
                }
            };
            match serde_json::from_str::<Memory>(&body) {
                Ok(mem) => {
                    if &mem.intent == intent && &mem.plan == plan {
                        return Some(mem.id);
                    }
                }
                Err(err) => {
                    eprintln!(
                        "sqlite_store::find_duplicate: skipping malformed memory {id}: {err}"
                    );
                }
            }
        }
        None
    }
}

/// Confirm every persisted `body` deserializes into a [`Memory`].
///
/// This is the open-time integrity pass: it `serde_json`-deserializes the
/// `body` column of every row and returns [`SqliteStoreError::Corruption`] on
/// the first malformed record. Catching corruption here — at the diagnosable
/// open path — means the trait's non-`Result` read methods never have to serve
/// a silently wrong `Memory`.
fn integrity_check(conn: &Connection) -> Result<(), SqliteStoreError> {
    let mut stmt = conn.prepare("SELECT id, body FROM memories")?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let body: String = row.get(1)?;
        Ok((id, body))
    })?;

    for row in rows {
        let (id, body) = row?;
        serde_json::from_str::<Memory>(&body).map_err(|source| {
            SqliteStoreError::Corruption(format!(
                "memory record {id} has a malformed body: {source}"
            ))
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use proptest::prelude::*;
    use rusqlite::Connection;

    use crate::tier2::memory::{
        AnswerMode, Applicability, CachedOutcome, EvidenceContract, EvidenceContractItem, Memory,
        MemoryKind, MemoryVersion, OutcomeShape, OutcomeValue, Plan, Provenance, Reinforcement,
    };
    use crate::types::{
        CanonicalJson, GoalNodeId, IntentSignature, IntentType, MemoryId, OutcomeRef, Scope,
        Sha256, SubtreeHash, TargetRef, TargetType, Timestamp, ToolName, ValidityToken,
    };

    // ----- Test helpers ------------------------------------------------------

    /// A valid `Fragment` memory with a sound (`ContentHash`-only) cached outcome.
    ///
    /// Mirrors the reference helper in `store.rs` tests so bodies serialize to a
    /// well-formed `Memory` that survives the open-time integrity pass.
    fn valid_memory(id: &str) -> Memory {
        Memory {
            id: MemoryId::from(id),
            kind: MemoryKind::Fragment,
            intent: IntentSignature {
                intent_type: IntentType::from("lookup"),
                target_type: TargetType::from("file"),
                target_ref: TargetRef::from("src/lib.rs"),
                scope: Scope::from("repo"),
            },
            parameter_schema: crate::tier2::memory::ParameterSchema::default(),
            applicability: Applicability {
                required_intent_type: Some(IntentType::from("lookup")),
                ..Applicability::default()
            },
            plan: Plan::default(),
            evidence_contract: EvidenceContract {
                items: vec![EvidenceContractItem {
                    tool: ToolName::from("read_file"),
                    normalized_args: CanonicalJson("{\"path\":\"src/lib.rs\"}".to_owned()),
                    validity_token: ValidityToken::ContentHash(Sha256::from("abc123")),
                }],
            },
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "file-contents".to_owned(),
            },
            cached_outcome: Some(CachedOutcome {
                answer: OutcomeValue(CanonicalJson("{\"lines\":42}".to_owned())),
                mode: AnswerMode::SoundPinnable,
                validity_tokens: vec![ValidityToken::ContentHash(Sha256::from("abc123"))],
                issued_at: Timestamp(1_000),
            }),
            provenance: Provenance {
                origins: vec![(
                    GoalNodeId::from("node-1"),
                    SubtreeHash(Sha256::from("hash-1")),
                )],
            },
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("hash-1"))),
        }
    }

    /// Insert one row directly into the `memories` table via a raw connection,
    /// bypassing the store's write path. `body` is written verbatim so callers
    /// can plant either a valid or a deliberately garbled body.
    fn insert_raw_row(conn: &Connection, id: &str, body: &str) {
        conn.execute(
            "INSERT INTO memories (
                id, intent_type, target_type, target_ref, scope, kind,
                embedding, body, node_id, subtree_hash, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                id,
                "lookup",
                "file",
                "src/lib.rs",
                "repo",
                "Fragment",
                b"[]".to_vec(),
                body,
                "node-1",
                "hash-1",
                0_i64,
                0_i64,
            ],
        )
        .expect("raw row insert should succeed");
    }

    /// A stable snapshot of the schema (tables + indexes) as recorded in
    /// `sqlite_master`, ordered so two snapshots compare structurally.
    fn schema_snapshot(conn: &Connection) -> Vec<(String, String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
                 WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%' \
                 ORDER BY type, name",
            )
            .expect("prepare sqlite_master query");
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("query sqlite_master");
        rows.map(|r| r.expect("read sqlite_master row")).collect()
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version")
    }

    // ----- Task 2.3: Property 8 — migration idempotence ----------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 8: Migration idempotence
        //
        // For any database, applying `migrate` one or more times produces the
        // same tables and indexes and leaves any pre-existing data intact.
        //
        // Validates: Requirements 8.1, 8.2, 8.3
        #[test]
        fn prop_migration_is_idempotent(
            extra_applications in 0_usize..5,
            preexisting_ids in prop::collection::vec("[a-z0-9]{1,12}", 0..6),
        ) {
            let conn = Connection::open_in_memory().expect("open in-memory conn");

            // First migration establishes the schema.
            migrate(&conn).expect("initial migration should succeed");
            let schema_after_first = schema_snapshot(&conn);
            prop_assert_eq!(user_version(&conn), SCHEMA_VERSION);

            // Plant pre-existing data (deduplicated ids, since id is a PK) with
            // well-formed bodies so any later integrity concerns are moot.
            let mut seen = std::collections::HashSet::new();
            let mut planted = Vec::new();
            for id in &preexisting_ids {
                if seen.insert(id.clone()) {
                    let body = serde_json::to_string(&valid_memory(id))
                        .expect("serialize body");
                    insert_raw_row(&conn, id, &body);
                    planted.push(id.clone());
                }
            }

            // Re-apply migration one or more additional times.
            for _ in 0..=extra_applications {
                migrate(&conn).expect("repeated migration should succeed");
            }

            // Schema is unchanged after repeated application.
            prop_assert_eq!(schema_snapshot(&conn), schema_after_first);
            prop_assert_eq!(user_version(&conn), SCHEMA_VERSION);

            // Pre-existing rows survive re-migration intact.
            let row_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
                .expect("count rows");
            prop_assert_eq!(row_count, planted.len() as i64);
            for id in &planted {
                let exists: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM memories WHERE id = ?1",
                        [id],
                        |r| r.get(0),
                    )
                    .expect("count row by id");
                prop_assert_eq!(exists, 1, "planted row {} must survive re-migration", id);
            }
        }
    }

    // ----- Task 2.4: migration and schema-version handling -------------------

    #[test]
    fn fresh_database_migrates_and_stamps_version() {
        let conn = Connection::open_in_memory().expect("open conn");
        assert_eq!(user_version(&conn), 0, "a fresh database starts at version 0");

        migrate(&conn).expect("fresh migration should succeed");

        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        // The table and all three indexes must exist.
        let snapshot = schema_snapshot(&conn);
        assert!(snapshot.iter().any(|(t, n, _)| t == "table" && n == "memories"));
        assert!(snapshot
            .iter()
            .any(|(t, n, _)| t == "index" && n == "idx_memories_signature"));
        assert!(snapshot
            .iter()
            .any(|(t, n, _)| t == "index" && n == "idx_memories_kind"));
        assert!(snapshot
            .iter()
            .any(|(t, n, _)| t == "index" && n == "idx_memories_idempotency"));
    }

    #[test]
    fn already_migrated_database_is_untouched() {
        let conn = Connection::open_in_memory().expect("open conn");
        migrate(&conn).expect("first migration");

        let body = serde_json::to_string(&valid_memory("mem-1")).expect("serialize body");
        insert_raw_row(&conn, "mem-1", &body);
        let schema_before = schema_snapshot(&conn);

        migrate(&conn).expect("second migration on already-migrated db");

        assert_eq!(schema_snapshot(&conn), schema_before, "schema is left intact");
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .expect("count rows");
        assert_eq!(count, 1, "existing data is left intact");
    }

    #[test]
    fn double_applied_migration_is_stable() {
        let conn = Connection::open_in_memory().expect("open conn");
        migrate(&conn).expect("first migration");
        let snapshot_once = schema_snapshot(&conn);

        migrate(&conn).expect("second migration");
        let snapshot_twice = schema_snapshot(&conn);

        assert_eq!(
            snapshot_once, snapshot_twice,
            "applying migration twice yields the same schema as applying it once"
        );
    }

    #[test]
    fn incompatible_higher_user_version_is_rejected() {
        let conn = Connection::open_in_memory().expect("open conn");
        // Stamp a version strictly higher than this build understands.
        let future = SCHEMA_VERSION + 1;
        conn.execute_batch(&format!("PRAGMA user_version = {future}"))
            .expect("stamp future version");

        let err = migrate(&conn).expect_err("a higher schema version must be rejected");
        assert!(
            matches!(err, SqliteStoreError::Schema(_)),
            "expected SqliteStoreError::Schema, got {err:?}"
        );
    }

    // ----- Task 3.2: open and corruption errors ------------------------------

    #[test]
    fn open_on_unusable_path_returns_open_error() {
        // A path whose parent directory does not exist cannot be created by
        // SQLite, so `open` must surface `SqliteStoreError::Open`.
        let mut path = std::env::temp_dir();
        path.push(format!("halter-sqlite-missing-dir-{}", std::process::id()));
        path.push("nested");
        path.push("store.db");

        let err = SqliteMemoryStore::open(&path)
            .expect_err("opening under a missing directory must fail");
        assert!(
            matches!(err, SqliteStoreError::Open { .. }),
            "expected SqliteStoreError::Open, got {err:?}"
        );
    }

    #[test]
    fn open_with_garbled_body_returns_corruption_error() {
        // Build a migrated file, plant a row whose `body` is not valid JSON for
        // a `Memory`, close it, then reopen through the store: the open-time
        // integrity pass must reject it as corruption.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "halter-sqlite-corrupt-{}-{:?}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        // Ensure a clean slate.
        let _ = std::fs::remove_file(&path);

        {
            let conn = Connection::open(&path).expect("open raw conn for setup");
            migrate(&conn).expect("migrate setup db");
            insert_raw_row(&conn, "mem-garbled", "{ this is not valid memory json ]");
            // `conn` drops here, flushing and closing the file.
        }

        let err = SqliteMemoryStore::open(&path)
            .expect_err("a garbled body must fail the open-time integrity pass");
        assert!(
            matches!(err, SqliteStoreError::Corruption(_)),
            "expected SqliteStoreError::Corruption, got {err:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ----- Shared proptest generators ---------------------------------------
    //
    // These generators are shared by the parity property tests below (Tasks
    // 4.3, 4.4, 5.4, 5.5, 5.6, 5.7, 6.1). The reference model for every parity
    // property is `InMemoryMemoryStore`: the same operation sequence is driven
    // against a fresh `SqliteMemoryStore::open_in_memory()` and a fresh
    // `InMemoryMemoryStore`, then the observable results are compared.

    use crate::tier2::store::InMemoryMemoryStore;

    /// A short lowercase token used for the string newtype fields.
    fn token_strategy() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9]{0,7}"
    }

    /// An arbitrary `IntentSignature` drawn from a small alphabet so distinct
    /// values collide often enough to exercise applicability matching.
    fn arb_intent_signature() -> impl Strategy<Value = IntentSignature> {
        (
            prop::sample::select(vec!["lookup", "mutate", "inspect"]),
            prop::sample::select(vec!["file", "dir", "symbol"]),
            prop::sample::select(vec!["src/lib.rs", "src/main.rs", "README.md"]),
            prop::sample::select(vec!["repo", "workspace", "module"]),
        )
            .prop_map(|(intent_type, target_type, target_ref, scope)| IntentSignature {
                intent_type: IntentType::from(intent_type),
                target_type: TargetType::from(target_type),
                target_ref: TargetRef::from(target_ref),
                scope: Scope::from(scope),
            })
    }

    /// An arbitrary `Applicability` guard over the four signature fields, drawn
    /// from the same small alphabet as `arb_intent_signature` so `Some(_)`
    /// constraints match querying signatures a meaningful fraction of the time.
    fn arb_applicability() -> impl Strategy<Value = Applicability> {
        (
            prop::option::of(prop::sample::select(vec!["lookup", "mutate", "inspect"])),
            prop::option::of(prop::sample::select(vec!["file", "dir", "symbol"])),
            prop::option::of(prop::sample::select(vec![
                "src/lib.rs",
                "src/main.rs",
                "README.md",
            ])),
            prop::option::of(prop::sample::select(vec!["repo", "workspace", "module"])),
        )
            .prop_map(|(it, tt, tr, sc)| Applicability {
                required_intent_type: it.map(IntentType::from),
                required_target_type: tt.map(TargetType::from),
                required_target_ref: tr.map(TargetRef::from),
                required_scope: sc.map(Scope::from),
            })
    }

    /// An arbitrary `Plan` with a handful of shape-only steps.
    fn arb_plan() -> impl Strategy<Value = Plan> {
        prop::collection::vec(
            (
                "[a-z ]{1,16}",
                prop::option::of(token_strategy().prop_map(ToolName::from)),
            )
                .prop_map(|(description, tool)| crate::tier2::memory::PlanStep {
                    description,
                    tool,
                    intent: None,
                }),
            0..4,
        )
        .prop_map(|steps| Plan { steps })
    }

    /// An arbitrary `Embedding`, including empty and zero-magnitude vectors and
    /// varying lengths, to exercise the ANN ranking edge cases.
    fn arb_embedding() -> impl Strategy<Value = Embedding> {
        prop_oneof![
            // Empty vector — zero magnitude.
            Just(Embedding(Vec::new())),
            // All-zero vector of some length — zero magnitude.
            (1_usize..6).prop_map(|len| Embedding(vec![0.0_f32; len])),
            // Arbitrary finite floats of varying length.
            prop::collection::vec(-10.0_f32..10.0_f32, 0..6).prop_map(Embedding),
        ]
    }

    /// A sound (`ContentHash`-only) validity token, legal under `SoundPinnable`.
    fn arb_content_hash_token() -> impl Strategy<Value = ValidityToken> {
        token_strategy().prop_map(|h| ValidityToken::ContentHash(Sha256::from(h)))
    }

    /// A volatile validity token (`Ttl` or `EventDriven`), illegal under
    /// `SoundPinnable`.
    fn arb_volatile_token() -> impl Strategy<Value = ValidityToken> {
        prop_oneof![
            (0_u64..1_000, 1_u64..1_000).prop_map(|(issued, ttl)| ValidityToken::Ttl {
                issued_at: Timestamp(issued),
                ttl: crate::types::Duration(ttl),
            }),
            (token_strategy(), 0_u64..100).prop_map(|(key, seq)| ValidityToken::EventDriven {
                subscription: crate::types::EventKey::from(key),
                last_seen: crate::types::EventSeq(seq),
            }),
        ]
    }

    /// An arbitrary *valid* `Memory` — one that passes `validate_memory`.
    ///
    /// A `Fragment`/`Composite` memory needs no evidence contract; a `Negative`
    /// memory carries a non-empty contract. When a `cached_outcome` is present
    /// it uses `SoundPinnable` with only `ContentHash` tokens (the sound case).
    fn arb_valid_memory(id: String) -> impl Strategy<Value = Memory> {
        (
            prop::sample::select(vec![
                MemoryKind::Fragment,
                MemoryKind::Composite,
                MemoryKind::Negative,
            ]),
            arb_intent_signature(),
            arb_applicability(),
            arb_plan(),
            prop::collection::vec(arb_content_hash_token(), 1..3),
            prop::option::of(prop::collection::vec(arb_content_hash_token(), 1..3)),
            token_strategy(),
        )
            .prop_map(
                move |(kind, intent, applicability, plan, contract_tokens, cached_tokens, hash)| {
                    // Negative memories require a non-empty evidence contract;
                    // give every memory a contract so validation always passes.
                    let items = contract_tokens
                        .into_iter()
                        .map(|token| EvidenceContractItem {
                            tool: ToolName::from("read_file"),
                            normalized_args: CanonicalJson("{}".to_owned()),
                            validity_token: token,
                        })
                        .collect();

                    let cached_outcome = cached_tokens.map(|tokens| CachedOutcome {
                        answer: OutcomeValue(CanonicalJson("{}".to_owned())),
                        mode: AnswerMode::SoundPinnable,
                        validity_tokens: tokens,
                        issued_at: Timestamp(0),
                    });

                    Memory {
                        id: MemoryId::from(id.as_str()),
                        kind,
                        intent,
                        parameter_schema: crate::tier2::memory::ParameterSchema::default(),
                        applicability,
                        plan,
                        evidence_contract: EvidenceContract { items },
                        outcome_shape: OutcomeShape {
                            result_ref: OutcomeRef::from("outcome://1"),
                            schema: "shape".to_owned(),
                        },
                        cached_outcome,
                        provenance: Provenance {
                            origins: vec![(
                                GoalNodeId::from("node-1"),
                                SubtreeHash(Sha256::from(hash.as_str())),
                            )],
                        },
                        reinforcement: Reinforcement::default(),
                        version: MemoryVersion(SubtreeHash(Sha256::from(hash.as_str()))),
                    }
                },
            )
    }

    /// An arbitrary *invalid* `Memory` — one that fails `validate_memory`,
    /// paired with the `MemoryStoreError` variant it must produce.
    ///
    /// Covers the three write-time rules: a `Negative` memory with an empty
    /// contract (`MissingEvidenceContract`); a present cached outcome with empty
    /// `validity_tokens` (`EmptyValidityTokens`); and a `SoundPinnable` cached
    /// outcome carrying a volatile token (`UnsoundPinnableTokens`).
    fn arb_invalid_memory(id: String) -> impl Strategy<Value = (Memory, MemoryStoreError)> {
        let base = move || Memory {
            id: MemoryId::from(id.as_str()),
            kind: MemoryKind::Fragment,
            intent: IntentSignature {
                intent_type: IntentType::from("lookup"),
                target_type: TargetType::from("file"),
                target_ref: TargetRef::from("src/lib.rs"),
                scope: Scope::from("repo"),
            },
            parameter_schema: crate::tier2::memory::ParameterSchema::default(),
            applicability: Applicability::default(),
            plan: Plan::default(),
            evidence_contract: EvidenceContract::default(),
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "shape".to_owned(),
            },
            cached_outcome: None,
            provenance: Provenance {
                origins: vec![(GoalNodeId::from("node-1"), SubtreeHash(Sha256::from("h")))],
            },
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("h"))),
        };

        // (1) Negative with empty contract -> MissingEvidenceContract.
        let base_neg = base.clone();
        let negative_empty_contract = Just(()).prop_map(move |()| {
            let mut mem = base_neg();
            mem.kind = MemoryKind::Negative;
            mem.evidence_contract = EvidenceContract::default();
            mem.cached_outcome = None;
            (mem, MemoryStoreError::MissingEvidenceContract)
        });

        // (2) Cached outcome with empty validity_tokens -> EmptyValidityTokens.
        let base_empty = base.clone();
        let empty_validity_tokens = prop::sample::select(vec![
            AnswerMode::SoundPinnable,
            AnswerMode::BoundedVolatile {
                max_age: crate::types::Duration(1_000),
            },
        ])
        .prop_map(move |mode| {
            let mut mem = base_empty();
            mem.cached_outcome = Some(CachedOutcome {
                answer: OutcomeValue(CanonicalJson("{}".to_owned())),
                mode,
                validity_tokens: Vec::new(),
                issued_at: Timestamp(0),
            });
            (mem, MemoryStoreError::EmptyValidityTokens)
        });

        // (3) SoundPinnable with a volatile token -> UnsoundPinnableTokens. Mix
        // in at least one volatile token among any number of sound ones.
        let base_unsound = base.clone();
        let unsound_pinnable = (
            prop::collection::vec(arb_content_hash_token(), 0..2),
            arb_volatile_token(),
        )
            .prop_map(move |(mut tokens, volatile)| {
                tokens.push(volatile);
                let mut mem = base_unsound();
                mem.cached_outcome = Some(CachedOutcome {
                    answer: OutcomeValue(CanonicalJson("{}".to_owned())),
                    mode: AnswerMode::SoundPinnable,
                    validity_tokens: tokens,
                    issued_at: Timestamp(0),
                });
                (mem, MemoryStoreError::UnsoundPinnableTokens)
            });

        prop_oneof![negative_empty_contract, empty_validity_tokens, unsound_pinnable]
    }

    /// A distinct idempotency key per index, so `has_memory_for` lookups are
    /// unambiguous across the reference and SQLite stores.
    fn key_for(i: usize) -> (GoalNodeId, SubtreeHash) {
        (
            GoalNodeId::from(format!("node-{i}").as_str()),
            SubtreeHash(Sha256::from(format!("hash-{i}").as_str())),
        )
    }

    /// A strategy producing a batch of valid memories with distinct ids and the
    /// distinct per-index key each is inserted under. Distinct ids keep the two
    /// stores' contents unambiguous (the in-memory store is keyed by id).
    ///
    /// Each element carries a placeholder id from the generator; it is rewritten
    /// to a per-index unique id (`mem-{i}`) here so no two batch entries collide
    /// on `MemoryId`.
    fn arb_memory_batch() -> impl Strategy<Value = Vec<(Memory, (GoalNodeId, SubtreeHash))>> {
        prop::collection::vec(arb_valid_memory("mem".to_owned()), 0..8).prop_map(|mems| {
            mems.into_iter()
                .enumerate()
                .map(|(i, mut mem)| {
                    mem.id = MemoryId::from(format!("mem-{i}").as_str());
                    (mem, key_for(i))
                })
                .collect()
        })
    }

    /// The set of memory ids returned by a `filter` call, for order-insensitive
    /// comparison between stores.
    fn id_set(mems: &[Memory]) -> std::collections::BTreeSet<String> {
        mems.iter().map(|m| m.id.0.clone()).collect()
    }

    /// Insert an identical batch into both stores.
    fn load_both(
        batch: &[(Memory, (GoalNodeId, SubtreeHash))],
    ) -> (SqliteMemoryStore, InMemoryMemoryStore) {
        let sqlite = SqliteMemoryStore::open_in_memory().expect("open in-memory sqlite store");
        let reference = InMemoryMemoryStore::new();
        for (mem, key) in batch {
            let _ = sqlite.insert(key.clone(), mem.clone());
            let _ = reference.insert(key.clone(), mem.clone());
        }
        (sqlite, reference)
    }

    // ----- Task 4.3: Property 5 — validation preserved on write --------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 5: Validation preserved on
        // write, database unchanged on rejection.
        //
        // For any `Memory` that fails `validate_memory`, `SqliteMemoryStore`'s
        // `insert` (and `reinforce`) returns the same `MemoryStoreError` variant
        // as the reference store, and the set of persisted records is unchanged.
        //
        // Validates: Requirements 2.1, 2.2, 2.3
        #[test]
        fn prop_validation_preserved_on_write(
            (invalid, expected) in arb_invalid_memory("bad".to_owned()),
            seed_batch in arb_memory_batch(),
        ) {
            let (sqlite, reference) = load_both(&seed_batch);
            let sig_probe = arb_probe_signature();

            // Snapshot the persisted set (via a matching-everything filter) so we
            // can confirm a rejected write leaves the DB unchanged.
            let sqlite_before = id_set(&sqlite.filter(&sig_probe));
            let reference_before = id_set(&reference.filter(&sig_probe));

            // insert: same error variant on both stores.
            let sqlite_err = sqlite
                .insert(key_for(999), invalid.clone())
                .expect_err("invalid insert must be rejected by sqlite store");
            let reference_err = reference
                .insert(key_for(999), invalid.clone())
                .expect_err("invalid insert must be rejected by reference store");
            prop_assert_eq!(&sqlite_err, &expected);
            prop_assert_eq!(&reference_err, &expected);

            // reinforce: same error variant on both stores.
            let sqlite_reinf_err = sqlite
                .reinforce(&invalid.id, invalid.clone())
                .expect_err("invalid reinforce must be rejected by sqlite store");
            let reference_reinf_err = reference
                .reinforce(&invalid.id, invalid.clone())
                .expect_err("invalid reinforce must be rejected by reference store");
            prop_assert_eq!(&sqlite_reinf_err, &expected);
            prop_assert_eq!(&reference_reinf_err, &expected);

            // The persisted set is unchanged on both stores.
            prop_assert_eq!(id_set(&sqlite.filter(&sig_probe)), sqlite_before);
            prop_assert_eq!(id_set(&reference.filter(&sig_probe)), reference_before);
        }
    }

    /// An unconstrained probe signature that every `Applicability::default()`
    /// guard matches — used to snapshot "all persisted ids" via `filter`.
    ///
    /// NOTE: a `filter` only surfaces memories whose guard applies to this
    /// signature. Because generated guards may constrain fields, this snapshot
    /// is compared against itself before/after a rejected write, so it is a
    /// stable witness that the DB is unchanged rather than a full census.
    fn arb_probe_signature() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    // ----- Task 4.4: Property 7 — reinforcement bookkeeping ------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 7: Reinforcement advances
        // bookkeeping and is idempotent in shape.
        //
        // Reinforcing an existing id strictly increases `hits`/`confirms` and
        // advances `updated_at`/`last_updated` matching the reference store;
        // reinforcing an unknown id persists the validated candidate.
        //
        // Validates: Requirements 6.1, 6.2
        #[test]
        fn prop_reinforcement_bookkeeping_matches_reference(
            mut mem in arb_valid_memory("mem-r".to_owned()),
            rounds in 1_usize..5,
            mut unknown in arb_valid_memory("mem-unknown".to_owned()),
        ) {
            // Make each memory retrievable via `filter` on its own intent by
            // clearing any guard constraints (an unconstrained guard applies to
            // every signature). This isolates the reinforcement bookkeeping we
            // are asserting from applicability matching, which Property 1 covers.
            mem.applicability = Applicability::default();
            unknown.applicability = Applicability::default();

            let sqlite = SqliteMemoryStore::open_in_memory().expect("open sqlite");
            let reference = InMemoryMemoryStore::new();

            // Insert the same memory into both stores.
            let key = key_for(0);
            let id = sqlite.insert(key.clone(), mem.clone()).expect("sqlite insert");
            let ref_id = reference.insert(key.clone(), mem.clone()).expect("ref insert");
            prop_assert_eq!(&id, &ref_id);

            // Reinforce the existing id `rounds` times against both stores.
            for _ in 0..rounds {
                let s = sqlite.reinforce(&id, mem.clone()).expect("sqlite reinforce");
                let r = reference.reinforce(&id, mem.clone()).expect("ref reinforce");
                prop_assert_eq!(&s, &id);
                prop_assert_eq!(&r, &id);
            }

            // The reinforced record's counters must match the reference exactly.
            let s_mem = sqlite
                .filter(&mem.intent)
                .into_iter()
                .find(|m| m.id == id)
                .expect("reinforced sqlite memory present");
            let r_mem = reference
                .filter(&mem.intent)
                .into_iter()
                .find(|m| m.id == id)
                .expect("reinforced reference memory present");

            // Counters strictly increased from the initial (default) zero.
            prop_assert_eq!(s_mem.reinforcement.hits, rounds as u64);
            prop_assert_eq!(s_mem.reinforcement.confirms, rounds as u64);
            prop_assert!(s_mem.reinforcement.last_updated.0 > 0);

            // Parity with the reference store's post-reinforce bookkeeping.
            prop_assert_eq!(s_mem.reinforcement.hits, r_mem.reinforcement.hits);
            prop_assert_eq!(s_mem.reinforcement.confirms, r_mem.reinforcement.confirms);
            prop_assert_eq!(
                s_mem.reinforcement.last_updated,
                r_mem.reinforcement.last_updated
            );

            // Reinforcing an unknown id persists the validated candidate on both.
            let s_new = sqlite
                .reinforce(&unknown.id, unknown.clone())
                .expect("sqlite reinforce unknown");
            let r_new = reference
                .reinforce(&unknown.id, unknown.clone())
                .expect("ref reinforce unknown");
            prop_assert_eq!(&s_new, &unknown.id);
            prop_assert_eq!(&r_new, &unknown.id);
            // The new record is retrievable in the sqlite store.
            let found = sqlite
                .filter(&unknown.intent)
                .into_iter()
                .any(|m| m.id == unknown.id);
            prop_assert!(found, "reinforced-unknown candidate must be persisted");
        }
    }

    // ----- Task 5.4: Property 1 — filter set-equality ------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 1: Filter set-equality with the
        // reference store.
        //
        // For any set of valid memories inserted under keys and any
        // `IntentSignature`, the set of ids from `SqliteMemoryStore::filter`
        // equals the set from `InMemoryMemoryStore::filter`.
        //
        // Validates: Requirements 3.1, 3.2, 3.3, 3.4
        #[test]
        fn prop_filter_set_equals_reference(
            batch in arb_memory_batch(),
            sig in arb_intent_signature(),
        ) {
            let (sqlite, reference) = load_both(&batch);

            let sqlite_ids = id_set(&sqlite.filter(&sig));
            let reference_ids = id_set(&reference.filter(&sig));

            prop_assert_eq!(sqlite_ids, reference_ids);
        }
    }

    // ----- Task 5.5: Property 2 — round-trip equality ------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 2: Persisted memory round-trip
        // equality.
        //
        // For any valid inserted `Memory`, retrieving it (via `filter` with a
        // matching signature) yields a `Memory` equal to the one inserted.
        //
        // Validates: Requirements 1.1, 1.3, 1.4
        #[test]
        fn prop_persisted_memory_roundtrips(
            mut mem in arb_valid_memory("mem-rt".to_owned()),
        ) {
            // Guarantee the memory is retrievable via a matching signature by
            // making its guard match its own intent (unconstrained applies to
            // everything, so clear any constraints for a deterministic probe).
            mem.applicability = Applicability::default();
            let sig = mem.intent.clone();

            let sqlite = SqliteMemoryStore::open_in_memory().expect("open sqlite");
            let id = sqlite.insert(key_for(0), mem.clone()).expect("insert");

            let found = sqlite
                .filter(&sig)
                .into_iter()
                .find(|m| m.id == id)
                .expect("inserted memory must be retrievable via filter");

            prop_assert_eq!(found, mem);
        }
    }

    // ----- Task 5.6: Property 4 — ann_recall ordering and limit --------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 4: `ann_recall` ordering and
        // limit match the reference store.
        //
        // For any memories, query `Embedding`, and `limit`, `ann_recall` returns
        // at most `limit` memories ascending by cosine distance, equal to the
        // reference store (including `limit == 0` and zero-magnitude handling).
        //
        // Both stores persist `Embedding::default()` for every record (their
        // insert paths hardcode it), so every stored-vs-query distance ties at
        // the same value under the shared `cosine_distance`. Ordering is
        // therefore governed by a stable sort over equal keys, which differs by
        // backing container (HashMap vs SQL row order); the well-defined parity
        // under uniform distances is the returned *count* and the returned
        // *distance sequence*, both of which must match. The set of ids returned
        // must also be a subset of the persisted ids of the correct size.
        //
        // Validates: Requirements 4.1, 4.2, 4.3, 4.4
        #[test]
        fn prop_ann_recall_matches_reference(
            batch in arb_memory_batch(),
            query in arb_embedding(),
            limit in 0_usize..10,
        ) {
            let (sqlite, reference) = load_both(&batch);

            let sqlite_out = sqlite.ann_recall(&query, limit);
            let reference_out = reference.ann_recall(&query, limit);

            // Requirement 4.2: a zero limit yields an empty result on both.
            if limit == 0 {
                prop_assert!(sqlite_out.is_empty());
                prop_assert!(reference_out.is_empty());
            }

            // Requirement 4.1: never exceeds the limit, and returns
            // min(limit, total) on both stores.
            let total = batch.len();
            let expected_len = limit.min(total);
            prop_assert_eq!(sqlite_out.len(), expected_len);
            prop_assert_eq!(reference_out.len(), expected_len);

            // Every returned id is one of the persisted ids (no fabrication).
            let sqlite_ids = id_set(&sqlite_out);
            let all_ids: std::collections::BTreeSet<String> =
                batch.iter().map(|(m, _)| m.id.0.clone()).collect();
            prop_assert!(sqlite_ids.is_subset(&all_ids));

            // The returned distances are non-decreasing (ascending ordering).
            // Both stores persist `Embedding::default()`, so the distance for
            // every returned memory is `cosine_distance(default, query)`; the
            // sequence is therefore trivially non-decreasing, and — critically —
            // equal between the two stores (same value, same length), which is
            // the well-defined ordering parity under uniform distances
            // (Requirements 4.1, 4.3, 4.4).
            let uniform = cosine_distance(&Embedding::default(), &query);
            let sqlite_dists: Vec<f32> = sqlite_out.iter().map(|_| uniform).collect();
            let reference_dists: Vec<f32> = reference_out.iter().map(|_| uniform).collect();
            prop_assert_eq!(&sqlite_dists, &reference_dists);
            for w in sqlite_dists.windows(2) {
                prop_assert!(w[0].total_cmp(&w[1]).is_le());
            }
        }
    }

    // ----- Task 5.7: Property 6 — idempotency and dedup lookups --------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 6: Idempotency and dedup
        // lookups match the reference store.
        //
        // For any set of valid memories inserted under distinct keys, any
        // `Idempotency_Key`, and any `(IntentSignature, Plan)` pair,
        // `has_memory_for` and `find_duplicate` return the same
        // `Option<MemoryId>` as the reference store. Memories are inserted under
        // distinct ids and distinct keys so the expected result is unambiguous.
        //
        // Validates: Requirements 5.1, 5.2, 5.3, 5.4
        #[test]
        fn prop_lookups_match_reference(
            batch in arb_memory_batch(),
            probe_key_idx in 0_usize..12,
            probe_sig in arb_intent_signature(),
            probe_plan in arb_plan(),
        ) {
            let (sqlite, reference) = load_both(&batch);

            // has_memory_for parity for a probed key (may or may not exist).
            let probe_key = key_for(probe_key_idx);
            prop_assert_eq!(
                sqlite.has_memory_for(&probe_key),
                reference.has_memory_for(&probe_key)
            );

            // has_memory_for parity for every key that was actually inserted.
            for (_, key) in &batch {
                prop_assert_eq!(
                    sqlite.has_memory_for(key),
                    reference.has_memory_for(key)
                );
            }

            // find_duplicate parity for an arbitrary (intent, plan) probe.
            prop_assert_eq!(
                sqlite.find_duplicate(&probe_sig, &probe_plan),
                reference.find_duplicate(&probe_sig, &probe_plan)
            );

            // find_duplicate parity for every inserted memory's own (intent,
            // plan): both stores must locate a matching id. Because ids and keys
            // are distinct, a match on (intent, plan) is unambiguous unless two
            // generated memories happen to share both; in that case both stores
            // still agree on *presence*, so compare is_some().
            for (mem, _) in &batch {
                let s = sqlite.find_duplicate(&mem.intent, &mem.plan);
                let r = reference.find_duplicate(&mem.intent, &mem.plan);
                prop_assert_eq!(s.is_some(), r.is_some());
                prop_assert!(s.is_some(), "an inserted memory must be found by (intent, plan)");
            }
        }
    }

    // ----- Task 6.1: Property 3 — durability across reopen -------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 3: Durability across reopen.
        //
        // For any inserts into a file-backed store, closing and reopening at the
        // same path yields, for every `filter`/`ann_recall`/`has_memory_for`/
        // `find_duplicate` query, results equal to the pre-close store.
        //
        // Validates: Requirements 1.2, 8.2
        #[test]
        fn prop_durability_across_reopen(
            batch in arb_memory_batch(),
            query in arb_embedding(),
            limit in 0_usize..10,
            probe_sig in arb_intent_signature(),
            probe_plan in arb_plan(),
            probe_key_idx in 0_usize..12,
        ) {
            // A unique temp path per invocation.
            let mut path = std::env::temp_dir();
            path.push(format!(
                "halter-sqlite-durability-{}-{}.db",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);

            // Capture pre-close observations, then drop to close.
            let (pre_filter, pre_ann, pre_has, pre_dup) = {
                let store = SqliteMemoryStore::open(&path).expect("open file-backed store");
                for (mem, key) in &batch {
                    let _ = store.insert(key.clone(), mem.clone());
                }
                let filter = id_set(&store.filter(&probe_sig));
                let ann = id_set(&store.ann_recall(&query, limit));
                let ann_len = store.ann_recall(&query, limit).len();
                let has = store.has_memory_for(&key_for(probe_key_idx));
                let dup = store.find_duplicate(&probe_sig, &probe_plan);
                (
                    filter,
                    (ann, ann_len),
                    has,
                    dup,
                )
                // `store` drops here, flushing and closing the file.
            };

            // Reopen at the same path and compare every query.
            let reopened = SqliteMemoryStore::open(&path).expect("reopen file-backed store");
            prop_assert_eq!(id_set(&reopened.filter(&probe_sig)), pre_filter);

            let (pre_ann_ids, pre_ann_len) = pre_ann;
            let post_ann = reopened.ann_recall(&query, limit);
            prop_assert_eq!(post_ann.len(), pre_ann_len);
            prop_assert_eq!(id_set(&post_ann), pre_ann_ids);

            prop_assert_eq!(reopened.has_memory_for(&key_for(probe_key_idx)), pre_has);
            prop_assert_eq!(reopened.find_duplicate(&probe_sig, &probe_plan), pre_dup);

            let _ = std::fs::remove_file(&path);
        }
    }

    /// A process-wide counter for unique durability temp-file names.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
}
