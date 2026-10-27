# Experiments

Reproducible measurements. Every number quoted in [the service README](../../README.md) and in
[`docs/README.md`](../README.md) came from one of these, and each is written so someone else can get
the same number — or notice when it stops reproducing.

| | what it answers |
|---|---|
| [cold-start.md](cold-start.md) | what the cache buys on a cold workspace |
| [read-batching.md](read-batching.md) | what sets read batch size, and what cannot measure it |
| [shared-output-base.md](shared-output-base.md) | whether several worktrees can share one output base |

Harnesses live under [`../../experiments/`](../../experiments/); nothing in CI runs them.

## What you need first

- **A patched Bazel.** See [`patches/README.md`](../../patches/README.md).
  `patches/verify.sh` checks each patch still matches its recorded revision before you
  start.
- **A token and a credential helper.** The service accepts only `Authorization: Bearer <jwt>`,
  where the JWT is signed by an issuer in `SKYCACHE_OIDC_ISSUERS` and carries the tenant claim.
  Any Bazel credential helper that returns that header works;
  [`local-credential-helper.sh`](../../experiments/local-credential-helper.sh) returns
  `SKYCACHE_TOKEN` for every request. Naming the wrong issuer refuses every token, and the refusal
  looks like a broken client rather than a misconfigured server.

- **A cluster.** `minikube start`, then `eval $(minikube docker-env) && skaffold run` from
  this directory.

## Three things that will silently ruin a measurement

**Rebuilding Bazel invalidates every entry.** `FrontierNodeVersion` folds in the install MD5, so a
download run must use the *same* binary that did the upload. A rebuild between the two legs looks
exactly like a cache that does not work. If a sweep reports 0% hits, suspect this
first.

**The local layers answer before the remote one.** A machine-wide `--disk_cache` and
`--noremote_upload_local_results` are common, and Bazel's own action cache sits in front of both. A
probe that does not disable all three measures the local disk, not the service: pass
`--disk_cache=`, `--remote_upload_local_results` and `--nouse_action_cache`. The last is not listed
by `bazel help build`.

**Suppress the clippy aspect on any download leg.** A workspace whose `.bazelrc` injects
`--aspects=…rust_clippy_aspect --output_groups=+clippy_checks` into every command hits this. The
aspect's `.clippy.ok` marker is an `ActionExecutionValue` the cache serves back with a *local* file
proxy — a ctime, mtime and inode from the machine that uploaded it — so requesting that output group
in a fresh output base crashes with `Missing artifact … lib.clippy.ok`. Nothing can materialise it:
it is a zero-byte marker, so no action cache has it either. Pass `--output_groups=-clippy_checks`.
Any requested output whose cached execution value carries a local proxy has the same shape;
analysis-only consumers are unaffected.
