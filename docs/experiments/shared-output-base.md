# One output base shared across worktrees

This does **not** give several worktrees one warm server. The output base belongs to one workspace
at a time, and pointing a second at it evicts the first. Six alternating invocations produce six
server PIDs:

```
for i in 1 2 3; do for W in <worktree-a> <worktree-b>; do
  (cd $W && bazel --output_base=/tmp/shared info server_pid)
done; done
```

Measure the cost of a switch with `--nobuild`, which stops after loading and analysis. That is the
quantity a shared output base destroys and the analysis cache restores; action results are keyed by
action rather than workspace, so including execution would add a constant to every arm and bury the
effect.

```
A=<worktree-a> B=<worktree-b> TARGET=//app/... ACTIVE=app \
  ../../experiments/switch-cost.sh                                                # control
A=... B=... TARGET=... ACTIVE=... SKYCACHE=grpc://$(minikube ip):30051 \
  HELPER=./local-credential-helper.sh ../../experiments/switch-cost.sh            # with the cache
```

## Without the analysis cache

On a Go tree of 212,242 targets, analysis only:

| arm | wall | targets configured | server restart |
|---|---:|---:|---|
| **shared base** — A cold | 31.1s | 212,242 | yes |
| **shared base** — B, after switch from A | 33.0s | 212,242 | yes |
| **shared base** — A, after switch back | 40.3s | 212,242 | yes |
| **shared base** — A again, no switch | **3.2s** | **0** | no |
| separate bases — A cold | 55.6s | 212,242 | yes |
| separate bases — B cold | 46.0s | 212,242 | yes |
| separate bases — A again, warm | **9.2s** | **0** | no |
| separate bases — B again, warm | **9.4s** | **0** | no |

Every switch re-configures all 212,242 targets. Staying put configures none. Separate output bases
pay that once per worktree; a shared one pays it on every alternation.

## With the analysis cache, and a remote action cache attached to both arms

Three worktree pairs of the same tree, analysis only, shared output base. The remote action cache
is attached in every arm below; it changes nothing, because action results are keyed by action
rather than by workspace.

| pair | leg | control | skycache | configured |
|---|---|---:|---:|---|
| A `main`, B feature branch | B after switch | 39.2s | **7.1s** | 212,242 → **13** |
| A `main`, B `main` @ same commit | B after switch | 42.9s | **7.5s** | 212,242 → **13** |
| A `main` @ new, B `main` @ 30 commits older | B after switch | 46.0s | **9.6s** | 212,242 → **13** |

Every leg reported `CONFIGURED_TARGET: 2/2 (100.00%)`, 3.94 MB over 4,988 lookups. The upload leg
costs 44–76s, once. Staying put costs 0.5–0.8s with the cache and 3.5–4.8s without.

**A switch becomes ~5x cheaper and is still ~10x worse than not switching.** The cache makes a
shared output base viable rather than preferable: you trade roughly 7s per switch for the memory
of N idle servers. If the goal is the memory and CPU contention those servers cause, the cheaper
lever is `--max_idle_secs` plus lower G1 `MinHeapFreeRatio`/`MaxHeapFreeRatio` via
`--host_jvm_args`, which return idle heap without destroying the analysis state that makes the next
build fast.

**The third row is not a robustness result. It is the unsoundness, measured.** The two worktrees
there hold genuinely different source: the control arm configures 212,764 targets in A against
212,242 in B. With the cache, B configured 13 and took the rest from entries A uploaded — a 100%
hit rate across 30 commits of divergence, with no re-analysis that could have noticed. That is
precisely what "invalidation is not implemented" means: the client reports every path as
unchanged, so a stale worktree is served a newer worktree's analysis and cannot tell. It is the
reason the service refuses to start without `--accept-unsound-invalidation`, and the reason these
numbers describe a prototype rather than something to point CI at.

Cross-worktree portability itself does hold, and is worth separating from the above: the branch,
the directory name and the checkout all differed across these pairs without costing a hit, because
`directories` and the build-data directory are position-indexed reference constants, so absolute
paths are never serialized.

Compare arms with care: the cold figures carry roughly ±40% run-to-run spread (31.1s against 55.6s
for the same work), so the load-bearing comparison is switch-versus-no-switch *within* an arm,
which is measured back to back.
