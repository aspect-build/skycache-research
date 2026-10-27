//! Cache effectiveness, as counters rather than log lines.
//!
//! Hit rate is the number that decides whether the cache is worth running, and it is only knowable
//! here: Bazel reports how many lookups it issued, not how many the service could answer.

use std::sync::LazyLock;

use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue, global};
use uuid::Uuid;

const METER: &str = "skycache";

static KEYS_REQUESTED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_keys_requested_total")
        .with_description("Keys asked for across all Get requests")
        .build()
});
static KEYS_HIT: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_keys_hit_total")
        .with_description("Keys a Get request could answer from storage")
        .build()
});
static GET_REQUESTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_get_requests_total")
        .with_description("Get RPCs served")
        .build()
});
static BYTES_READ: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_bytes_read_total")
        .with_description("Entry bytes returned to clients")
        .build()
});
static ENTRIES_OFFERED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_entries_offered_total")
        .with_description("Entries sent in Put requests, before deduplication")
        .build()
});
static ENTRIES_STORED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_entries_stored_total")
        .with_description("Entries a Put request actually inserted")
        .build()
});
static PUT_REQUESTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_put_requests_total")
        .with_description("Put RPCs served")
        .build()
});
static BYTES_WRITTEN: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_bytes_written_total")
        .with_description("Entry bytes accepted from clients, before deduplication")
        .build()
});
static REJECTED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER)
        .u64_counter("skycache_requests_rejected_total")
        .with_description("Requests refused before reaching storage")
        .build()
});

/// Forces the counters to exist before the first request, so a scrape of an idle pod reports zero
/// rather than nothing and an alert can tell "no hits" from "not running".
pub fn initialize() {
    let tenant = [KeyValue::new("tenant", "")];
    KEYS_REQUESTED.add(0, &tenant);
    KEYS_HIT.add(0, &tenant);
    GET_REQUESTS.add(0, &tenant);
    BYTES_READ.add(0, &tenant);
    ENTRIES_OFFERED.add(0, &tenant);
    ENTRIES_STORED.add(0, &tenant);
    PUT_REQUESTS.add(0, &tenant);
    BYTES_WRITTEN.add(0, &tenant);
    REJECTED.add(0, &[KeyValue::new("reason", "")]);
}

/// Series are per tenant, which bounds their count by the tenant map rather than by anything a
/// caller controls.
fn labels(tenant: Uuid) -> [KeyValue; 1] {
    [KeyValue::new("tenant", tenant.to_string())]
}

pub fn record_get(tenant: Uuid, requested: usize, hit: usize, bytes: usize) {
    let labels = labels(tenant);
    GET_REQUESTS.add(1, &labels);
    KEYS_REQUESTED.add(requested as u64, &labels);
    KEYS_HIT.add(hit as u64, &labels);
    BYTES_READ.add(bytes as u64, &labels);
}

pub fn record_put(tenant: Uuid, offered: usize, stored: u64, bytes: usize) {
    let labels = labels(tenant);
    PUT_REQUESTS.add(1, &labels);
    ENTRIES_OFFERED.add(offered as u64, &labels);
    ENTRIES_STORED.add(stored, &labels);
    BYTES_WRITTEN.add(bytes as u64, &labels);
}

pub fn record_rejected(reason: &'static str) {
    REJECTED.add(1, &[KeyValue::new("reason", reason)]);
}
