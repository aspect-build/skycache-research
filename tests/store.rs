//! Storage behaviour against a real Postgres.
//!
//! Unordered concurrent upserts deadlock, and `stored` is the only signal a client has that
//! deduplication happened.

use std::time::Duration;

use miette::{IntoDiagnostic, Result};
use skycache::db::{CredentialSource, DbEndpoint, DbSslMode, new_pool};
use skycache::store::{Store, StoredEntry};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);

async fn store_against_container() -> Result<(Store, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await.into_diagnostic()?;
    let host = container.get_host().await.into_diagnostic()?.to_string();
    let port = container.get_host_port_ipv4(5432).await.into_diagnostic()?;

    let endpoint = DbEndpoint::from_flags(
        &format!("{host}:{port}"),
        "postgres".to_string(),
        "postgres".to_string(),
        DbSslMode::Disable,
    )?;
    let pool = new_pool(
        endpoint,
        CredentialSource::Static("postgres".to_string()),
        4,
    )
    .await?;
    let store = Store::from_pool(pool);
    store.migrate().await?;
    Ok((store, container))
}

fn entry(key: &[u8], value: &[u8]) -> StoredEntry {
    StoredEntry {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[tokio::test]
async fn a_value_survives_a_round_trip_byte_for_byte() -> Result<()> {
    let (store, _container) = store_against_container().await?;

    // A stored entry is an opaque blob carrying Bazel's own framing. The service must not interpret
    // it, so the test asserts on bytes that would break any parser: a leading zero, an embedded NUL
    // and a trailing high byte.
    let value = [0x00, 0x01, 0x00, 0xff, b'v', b'a', b'l', 0xff];
    assert_eq!(store.put(TENANT, &[entry(b"k1", &value)]).await?, 1);

    let found = store.get(TENANT, &[b"k1".to_vec()]).await?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, b"k1");
    assert_eq!(found[0].value, value);
    Ok(())
}

#[tokio::test]
async fn re_putting_a_key_stores_nothing_and_keeps_the_first_value() -> Result<()> {
    let (store, _container) = store_against_container().await?;

    assert_eq!(store.put(TENANT, &[entry(b"k", b"first")]).await?, 1);
    assert_eq!(store.put(TENANT, &[entry(b"k", b"second")]).await?, 0);

    let found = store.get(TENANT, &[b"k".to_vec()]).await?;
    assert_eq!(found[0].value, b"first");
    Ok(())
}

#[tokio::test]
async fn a_duplicate_key_within_one_batch_counts_once() -> Result<()> {
    let (store, _container) = store_against_container().await?;

    // Without the in-batch dedup, ON CONFLICT DO NOTHING raises "cannot affect row a second time".
    let stored = store
        .put(TENANT, &[entry(b"dup", b"a"), entry(b"dup", b"b")])
        .await?;
    assert_eq!(stored, 1);
    Ok(())
}

#[tokio::test]
async fn overlapping_concurrent_batches_do_not_deadlock() -> Result<()> {
    let (store, _container) = store_against_container().await?;

    // The same keys in opposite orders is exactly what deadlocked before writes were sorted. Every
    // uploading build re-sends shared entries, so this is the common case, not a rare race.
    let forward: Vec<StoredEntry> = (0..200u32).map(|i| entry(&i.to_be_bytes(), b"v")).collect();
    let backward: Vec<StoredEntry> = (0..200u32)
        .rev()
        .map(|i| entry(&i.to_be_bytes(), b"v"))
        .collect();

    let a = store.clone();
    let b = store.clone();
    let (first, second) = tokio::join!(
        tokio::spawn(async move { a.put(TENANT, &forward).await }),
        tokio::spawn(async move { b.put(TENANT, &backward).await }),
    );
    let first = first.into_diagnostic()??;
    let second = second.into_diagnostic()??;

    assert_eq!(first + second, 200, "every key stored exactly once");
    assert_eq!(
        store
            .get(TENANT, &[0u32.to_be_bytes().to_vec()])
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn tenants_do_not_see_each_other() -> Result<()> {
    let (store, _container) = store_against_container().await?;

    store.put(TENANT, &[entry(b"shared", b"mine")]).await?;
    let other = store.get(OTHER_TENANT, &[b"shared".to_vec()]).await?;
    assert!(other.is_empty(), "one tenant read another tenant's entry");

    // The same key under a second tenant is a new row, not a conflict with the first.
    assert_eq!(
        store
            .put(OTHER_TENANT, &[entry(b"shared", b"theirs")])
            .await?,
        1
    );
    let mine = store.get(TENANT, &[b"shared".to_vec()]).await?;
    assert_eq!(mine[0].value, b"mine");
    Ok(())
}

#[tokio::test]
async fn absent_keys_are_omitted_rather_than_returned_empty() -> Result<()> {
    let (store, _container) = store_against_container().await?;

    store.put(TENANT, &[entry(b"present", b"v")]).await?;
    let found = store
        .get(TENANT, &[b"present".to_vec(), b"absent".to_vec()])
        .await?;

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, b"present");
    Ok(())
}

#[tokio::test]
async fn migrate_is_safe_to_run_concurrently() -> Result<()> {
    let (store, _container) = store_against_container().await?;

    // More than one replica starts at once in a rollout, and concurrent CREATE TABLE IF NOT EXISTS
    // races on pg_type rather than no-opping.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let store = store.clone();
        handles.push(tokio::spawn(async move { store.migrate().await }));
    }
    for handle in handles {
        handle.await.into_diagnostic()??;
    }
    Ok(())
}

/// The failure this was written for: a reachable database with no schema. `SELECT 1` succeeds
/// against it, so a connectivity-only probe reported the pod Ready while every write failed on a
/// missing relation — 1,066 of them in one upload before anyone noticed.
#[tokio::test]
async fn ping_fails_when_the_schema_is_missing_not_just_the_database() -> Result<()> {
    let (store, _container) = store_against_container().await?;
    store.ping().await?;

    let mut conn = store.pool().get().await.into_diagnostic()?;
    diesel_async::RunQueryDsl::execute(
        diesel::sql_query("DROP TABLE analysis_cache_entries"),
        &mut conn,
    )
    .await
    .into_diagnostic()?;
    drop(conn);

    assert!(
        store.ping().await.is_err(),
        "ping passed against a database with no schema"
    );
    // Idempotent, so the health gate can use it to recover rather than serve errors forever.
    store.migrate().await?;
    store.ping().await?;
    Ok(())
}

#[tokio::test]
async fn ping_fails_once_the_database_is_gone() -> Result<()> {
    let (store, container) = store_against_container().await?;
    store.ping().await?;

    container.stop().await.into_diagnostic()?;
    // The pool hands out a cached connection first; the ping has to actually reach Postgres for the
    // health gate to mean anything.
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if store.ping().await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(result.is_ok(), "ping never reported the database as gone");
    Ok(())
}
