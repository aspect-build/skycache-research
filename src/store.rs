//! Postgres-backed entry storage.
//!
//! The workload is many tiny rows, so every statement here handles a whole request's worth of keys
//! in one round trip.

use diesel::result::DatabaseErrorKind;
use diesel::sql_types::{Array, Bytea, Uuid as SqlUuid};
use diesel::{QueryableByName, sql_query};
use diesel_async::RunQueryDsl;
use miette::{IntoDiagnostic, Result, WrapErr};
use uuid::Uuid;

use crate::db::{CredentialSource, DbEndpoint, DbPool, new_pool};

/// Applied at startup.
///
/// The tenant is the first primary-key column so a tenant's entries are contiguous in the index:
/// every statement here filters on exactly one tenant, and none ever spans two.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS analysis_cache_entries (
    tenant      UUID   NOT NULL,
    key         BYTEA  NOT NULL,
    value       BYTEA  NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, key)
);
";

#[derive(QueryableByName)]
struct EntryRow {
    #[diesel(sql_type = Bytea)]
    key: Vec<u8>,
    #[diesel(sql_type = Bytea)]
    value: Vec<u8>,
}

#[derive(Clone)]
pub struct Store {
    pool: DbPool,
}

pub struct StoredEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

impl Store {
    pub async fn connect(
        endpoint: DbEndpoint,
        credentials: CredentialSource,
        max_connections: u32,
    ) -> Result<Self> {
        let store = Self {
            pool: new_pool(endpoint, credentials, max_connections).await?,
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Wraps an existing pool, for tests that build one against a container.
    pub fn from_pool(pool: DbPool) -> Self {
        Self { pool }
    }

    /// The underlying pool, so a test can reach past the store's own statements.
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Creates the table if it is absent.
    ///
    /// Safe to run from several replicas at once: concurrent `CREATE TABLE IF NOT EXISTS` can lose
    /// the race on `pg_type` and raise a duplicate-key error rather than the clean no-op the name
    /// suggests, so that specific failure is treated as success.
    pub async fn migrate(&self) -> Result<()> {
        let mut conn = self.pool.get().await.into_diagnostic()?;
        match sql_query(SCHEMA).execute(&mut conn).await {
            Ok(_) => Ok(()),
            Err(e) if is_duplicate_object(&e) => Ok(()),
            Err(e) => Err(e).into_diagnostic().wrap_err("applying the schema"),
        }
    }

    /// Fetches the entries present for `keys`. Absent keys are simply not returned.
    pub async fn get(&self, tenant: Uuid, keys: &[Vec<u8>]) -> Result<Vec<StoredEntry>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.get().await.into_diagnostic()?;
        let rows: Vec<EntryRow> = sql_query(
            "SELECT key, value FROM analysis_cache_entries \
             WHERE tenant = $1 AND key = ANY($2)",
        )
        .bind::<SqlUuid, _>(tenant)
        .bind::<Array<Bytea>, _>(keys)
        .get_results(&mut conn)
        .await
        .into_diagnostic()
        .wrap_err("reading entries")?;

        Ok(rows
            .into_iter()
            .map(|row| StoredEntry {
                key: row.key,
                value: row.value,
            })
            .collect())
    }

    /// Stores entries, ignoring any key already present, and reports how many were new.
    ///
    /// Re-sending a known key is the normal case rather than an error: the key is a fingerprint of
    /// the value, so every build uploads entries other builds may already have contributed.
    pub async fn put(&self, tenant: Uuid, entries: &[StoredEntry]) -> Result<u64> {
        if entries.is_empty() {
            return Ok(0);
        }

        // Insert in key order. Two concurrent batches that share keys take row locks in whatever
        // order their rows arrive, and opposite orders deadlock. Sorting gives all writers one lock
        // order. Duplicates within a batch are dropped for the same reason.
        let mut ordered: Vec<&StoredEntry> = entries.iter().collect();
        ordered.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        ordered.dedup_by(|a, b| a.key == b.key);

        let keys: Vec<Vec<u8>> = ordered.iter().map(|e| e.key.clone()).collect();
        let values: Vec<Vec<u8>> = ordered.iter().map(|e| e.value.clone()).collect();

        let mut conn = self.pool.get().await.into_diagnostic()?;
        let stored = sql_query(
            "INSERT INTO analysis_cache_entries (tenant, key, value) \
             SELECT $1, k, v FROM UNNEST($2::bytea[], $3::bytea[]) AS t(k, v) \
             ON CONFLICT (tenant, key) DO NOTHING",
        )
        .bind::<SqlUuid, _>(tenant)
        .bind::<Array<Bytea>, _>(keys)
        .bind::<Array<Bytea>, _>(values)
        .execute(&mut conn)
        .await
        .into_diagnostic()
        .wrap_err("writing entries")?;

        Ok(stored as u64)
    }

    /// Checks the pool can reach Postgres *and* that the schema is there.
    ///
    /// Touching the table rather than `SELECT 1` is the point: a reachable database with no
    /// schema answers `SELECT 1` happily, so a connectivity-only probe reports a pod Ready while
    /// every write fails on a missing relation. That is not hypothetical — migrating against a
    /// Postgres still running its init left exactly that state, and the pod served errors until
    /// it was restarted by hand.
    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.pool.get().await.into_diagnostic()?;
        sql_query("SELECT 1 FROM analysis_cache_entries LIMIT 1")
            .execute(&mut conn)
            .await
            .into_diagnostic()
            .wrap_err("pinging")?;
        Ok(())
    }
}

/// Postgres reports the lost race either as a unique violation on a catalogue table or, for
/// `42710 duplicate_object`, as an unclassified error whose message is the only distinguishing
/// mark. Diesel exposes no SQLSTATE, so the message is what there is to match on.
fn is_duplicate_object(error: &diesel::result::Error) -> bool {
    match error {
        diesel::result::Error::DatabaseError(DatabaseErrorKind::UniqueViolation, _) => true,
        diesel::result::Error::DatabaseError(_, info) => info.message().contains("already exists"),
        _ => false,
    }
}
