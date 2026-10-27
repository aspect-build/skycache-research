# skycache: design, and what is actually known

[`../README.md`](../README.md) says what the service is. This says how it fits together, what has
been measured, and what has not.

- [experiments/](experiments/) — reproducible measurements, and how to re-run them
- [../patches/](../patches/) — the client patch, one directory per Bazel release

## Architecture

```
patched bazel ──gRPC──> skycache ──> Postgres
     │                     │
     │ credential helper   │ verifies the bearer against a trusted OIDC
     │ supplies the bearer │ issuer, reads the tenant from its claims
     │
     └─ action cache ────> any remote cache   (orthogonal, see below)
```

**Two caches, different layers, no interaction.** skycache stores *analysis* — the serialized
Skyframe graph. A remote action cache stores *actions*. Action results are keyed by action, not by
workspace, so attaching the remote cache changes none of the analysis-side numbers; the
[shared-output-base measurement](experiments/shared-output-base.md) ran every arm with it attached
to show exactly that.

**Bazel cannot reach this unpatched.** Stock Bazel serializes its analysis graph but has nowhere
to put it and no seam to add one: `SerializationModule` hard-wires an in-memory store, and
`RemoteAnalysisCacheClient` and `LongVersionGetter` have no implementations.

**Both directions are batched**, because both of Bazel's paths move one entry at a time: an upload
of the measured tree makes ~600,000 single-entry writes and a hitting download ~5,000 lookups. A
linger, not the size cap, is what decides read batch size — see
[read batching](experiments/read-batching.md).

**One read queue serves both read paths.** The invalidation-data header that only one of them
wants removed is removed *on the client*, because the value store also fetches nested shared
values that never had one. Sharing the queue is what keeps the batches full.

**The tenant is proven, never asserted.** Nothing names a tenant on the wire. The service verifies
the bearer against a trusted OIDC issuer and reads the tenant from its claims. Rows are keyed
`(tenant, key)`; a token with no usable tenant claim is refused rather than defaulted. There is no
single-tenant mode and no local stand-in for the issuer.

**Entries are small** — 365k per upload, median 254 bytes, 98.5% under 512 — which is why this is
Postgres rows and not objects in a bucket.

## What is known

- **The cache works, and the win is real.** A cold workspace evaluates 13 targets against 212,244,
  in 18.8s against 126.9s. [cold-start.md](experiments/cold-start.md)
- **Entries are portable across worktrees.** Branch, directory name and checkout all differed
  across three measured pairs without costing a hit, because `directories` and the build-data
  directory are position-indexed reference constants — absolute paths are never serialized.
- **A shared output base is the wrong lever.** The base belongs to one workspace at a time, so
  every switch evicts the server and re-analyzes. The cache takes that from ~40s to ~7s, still far
  worse than the ~0.5s of not switching. [shared-output-base.md](experiments/shared-output-base.md)

## What is not known

- **Whether a hit is ever *correct* across differing source.** It is currently correct only by
  accident: invalidation is not implemented, so nothing checks. A worktree 30 commits behind
  another was served the newer tree's analysis at a 100% hit rate with no re-analysis that could
  have noticed. This is the single most important open question and it gates everything else.
- **Whether the measured numbers hold at real scale.** Every figure here comes from one
  developer machine against minikube, with ±30–40% run-to-run spread on cold legs. Nothing has run
  on CI hardware, against a managed database, or with more than one concurrent client.
- **What the upload costs in practice.** Measured at 44–76s for one target's frontier on a warm
  tree; never measured against a cold CI runner, where it competes with the build it is meant to
  speed up.
- **Whether `--experimental_active_directories` scoping is right.** The frontier boundary decides
  what gets serialized, and the only setting exercised here is "the target's own tree".

## Open items

**Invalidation — the blocker.** `LongVersionGetter` answers when a path last changed; inside
Google that comes from the version control snapshot. This implementation reports every path as
unchanged. Until versions derive from something that tracks content, every number here is "fast
and possibly wrong", and `--accept-unsound-invalidation` is the only thing standing between it and
someone pointing CI at it. This is the actual product.

**Split the store by role.** Index in Postgres (fingerprint → content digest), values elsewhere.
Object storage or a store whose batch semantics match small records is the candidate for
"elsewhere". Note the client would newly have to hash every value on write — measured at ~52k
blobs/s, so ~7s for 365k entries, real but not the problem.

**Execution values carry local file proxies.** A cached `ActionExecutionValue` can reference an
artifact by the uploading machine's ctime/mtime/inode. Requesting such an output in a fresh output
base fails and nothing can materialise it. Today this is avoided by not requesting those outputs;
it is not fixed.

**No end-to-end coverage.** Nothing in CI exercises a patched Bazel against a deployed service, so
no check would catch the client and service disagreeing on the wire. All measurements are by hand.

**Bazel 10 is a release candidate.** Patches exist for 9.2.0, 9.3.1 and 10.0.0rc2
([`../patches/`](../patches/)). The 10.0 series rebases onto 10.0.0 once it is tagged, and carries
a revert of upstream's no-op `--experimental_remote_analysis_cache` for as long as upstream keeps
it out.
