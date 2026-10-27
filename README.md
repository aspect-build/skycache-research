# skycache

Stores Bazel's serialized Skyframe analysis graph so a cold workspace can download it instead of
re-analyzing. On a Go tree of 212,244 targets, a download evaluates 13 of them where a cold build
evaluates all of them, and finishes in 18.8s against 126.9s.

**Experimental, and unsound in a way that matters.** Bazel's invalidation oracle has no
open-source implementation, so the patched client reports every source path as unchanged: an entry
stays a hit after the files it was derived from have moved. That produces wrong builds, not slow
ones. The binary refuses to start without `--accept-unsound-invalidation`.

This is measured, not theoretical — a worktree 30 commits behind another was served the newer
tree's analysis at a 100% hit rate, with no re-analysis that could have noticed. Deriving versions
from something that tracks content is the next milestone, and the actual product.

Stock Bazel cannot talk to this; the client patch is in [`patches/`](patches/), one
directory per supported release.

## Where things are

| | |
|---|---|
| [`docs/`](docs/) | architecture, what is known and not known, open items |
| [`docs/experiments/`](docs/experiments/) | reproducible measurements, and how to re-run them |
| [`patches/`](patches/) | the client patch, per Bazel release |
| [`k8s/`](k8s/) | manifests |
| [`experiments/`](experiments/) | measurement scripts; nothing CI runs |

## Building

This directory is its own Bazel module and Cargo workspace; run everything from here.

```
bazel build //:skycache
bazel test //:lib_test //:store_test //:service_test //patches:patch_base_test
```

`Cargo.lock` is the version source for Bazel too: after changing `Cargo.toml`, refresh it with
`cargo metadata --format-version=1 >/dev/null` and commit both.

## Running it locally

Set `SKYCACHE_OIDC_ISSUERS` in [`k8s/skycache.yaml`](k8s/skycache.yaml) to the issuer of the tokens
you will present, then:

```
eval $(minikube docker-env)
skaffold dev
```

Then point a patched Bazel at it, with a JWT from that issuer:

```
SKYCACHE_TOKEN=<jwt> bazel build //some/target \
  --experimental_remote_analysis_cache_mode=UPLOAD \
  --experimental_remote_analysis_cache=grpc://$(minikube ip):30051 \
  --credential_helper="$(minikube ip)=<skycache>/experiments/local-credential-helper.sh" \
  --experimental_active_directories=some
```

`grpc://` matters: a bare `host:port` means TLS, which the dev manifests do not terminate. A
`NodePort` rather than `kubectl port-forward`, because an upload pushes hundreds of thousands of
entries through a forward that multiplexes everything onto one connection and collapses.

[`docs/experiments/`](docs/experiments/) has the prerequisites in full, including the three things
that will silently ruin a measurement.

## Metrics

Hit rate is only knowable here — Bazel reports lookups issued, not lookups answered — so
`skycache_keys_requested_total` and `skycache_keys_hit_total` are the pair worth watching,
labelled by tenant. `skycache_requests_rejected_total` is labelled by reason and separates a
caller with no token from one whose token carries no tenant.

## Tests

`store_test` and `service_test` run Postgres in a container, so they need Docker on the host. A
green local run used a warm image cache; a fresh machine pulls cold. `service_test`
goes through the authentication layer against a mock OIDC issuer rather than constructing the
service directly, so what it pins is what a client can actually reach.

Measurements are not tests and nothing in CI reproduces them.
