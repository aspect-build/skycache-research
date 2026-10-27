# Bazel patch

The service is useless without this. Bazel can serialize its Skyframe analysis graph, but stock
Bazel has nowhere to put it: `SerializationModule` hard-wires an in-memory `FingerprintValueStore`,
`RemoteAnalysisCacheClient` has no implementation, and `LongVersionGetter` has none either — its
absence crashes an upload with `NullPointerException("missing versionGetter")`. There is no plugin
seam, so a patched binary is the only way in.

## Which Bazel

One directory per supported release, named for it, holding a patch series and the `BAZEL_VERSION`
that records the revision it was generated against:

```
patches/
  verify.sh
  9.2.0/
    BAZEL_VERSION
    0001-Point-remote-analysis-caching-at-an-external-cache-s.patch
$1  10.0.0rc2/
    BAZEL_VERSION
    0001-Restore-experimental_remote_analysis_cache.patch
    0002-Point-remote-analysis-caching-at-an-external-cache-s.patch
```

The first patch carries the same commit as a `base-commit:` trailer. `verify.sh` pins the three
records of that revision to each other — the trailer, `BAZEL_VERSION`, and the directory name — and
runs as `//patches:patch_base_test`, one test per version so a failure names it. Pass a Bazel
checkout to also check each series still applies:

```
./verify.sh                                  # every version
./verify.sh --only 9.3.1                     # one
./verify.sh ~/src/github/bazelbuild/bazel    # and check it applies
```

Regenerating a patch means updating its `BAZEL_VERSION` in the same commit; nothing about a stale
base looks wrong until `git am` fails.

### Adding a version

Create `patches/<release>/`, port the series, and write its `BAZEL_VERSION`. The test picks the
directory up from a glob, so nothing else needs editing. Within a major version a port is a rebase
plus whatever signatures moved. Across one expect a real port; see "Rebasing it" below.

## Applying it

Against the release `BAZEL_VERSION` names:

```
git clone https://github.com/bazelbuild/bazel
cd bazel
git checkout 9.3.1
git am <skycache>/patches/9.3.1/*.patch
bazel build //src:bazel --embed_label=9.3.1 --stamp
```

**The stamp is not optional.** Without it the binary reports `no_version`, `bazel_features` then
reports every version-gated feature as unavailable, and rulesets silently take legacy code
paths — which fails far from the cause, in `cc_library` analysis.

## Running against it

```
bazel build //some/target \
  --experimental_remote_analysis_cache_mode=UPLOAD \
  --experimental_remote_analysis_cache=grpcs://<host:port> \
  --credential_helper="<host>=<helper>" \
  --experimental_active_directories=<dir>
```

Download mode takes the same endpoint and no active directories. Uploads need a cold Skyframe:
`BuildView.UPLOAD_BUILDS_MUST_BE_COLD` is a hard error on a warm server.

**Authentication is Bazel's credential helper, and the patch adds no flag for it.** The helper must
return `Authorization: Bearer <jwt>`; it shares the credential cache with the remote module, so it
runs once per URI per build rather than once per consumer. The channel is built by
`GoogleAuthUtils.newChannel`, so TLS follows the target's scheme exactly as it does for the remote
cache: a bare `host:port` or `grpcs://` means TLS, `grpc://` means plaintext.

There is no namespace or tenant flag either, and deliberately so. The service resolves the tenant
from the verified token; a client that could name a namespace could name someone else's. Nothing
else needs separating — `FrontierNodeVersion` already salts every top-level key with the install
MD5 and the top-level configuration checksum, and shared values are addressed by a fingerprint of
bytes the codec registry produced, so two incompatible Bazel installs cannot collide on a key.

**Rebuilding the binary invalidates every entry**, for the same reason: the frontier version folds
in the install MD5 (`BuildTool.java`), so entries written by one build of Bazel are unreadable by
the next, even from identical source. Expect a full miss after any rebuild; pin it with
`--experimental_analysis_cache_server_checksum_override` if you need entries to survive one.

## Batch tuning

`--experimental_remote_analysis_cache_get_linger` (default 25ms) sets how long a partial read batch
waits. It, not the batch size cap, decides batch size in practice. Measured on a cold worktree
against a service on minikube, 4,988 lookups each time:

| linger | wall | Get RPCs | mean keys | max |
|-------:|-----:|---------:|----------:|----:|
| 1 ms | 6.7s | 293 | 17.0 | 139 |
| 5 ms | 7.1s | 127 | 39.3 | 238 |
| **25 ms** | **6.5s** | **32** | **155.9** | 715 |
| 100 ms | 7.5s | 27 | 184.7 | 1024 |

Past 25ms the round trips stop falling while the wait costs more than it saves. Raise it where the
network between client and service is slower than a loopback.

Batching on RPC completion instead — which mainline's target-concurrency option and absent linger
suggest it does — was built and measured and is worse here, issuing 1.6x to 5x more RPCs at every
injected latency with no wall-time difference. Skyframe's reads are a closed loop: the demand for
the next key comes from a node the previous response unblocked, so the queue cannot fill while the
RPC that would fill it is outstanding.

The other knobs are Bazel's own: `--experimental_remote_analysis_cache_max_batch_size` for writes,
`--experimental_remote_analysis_cache_reader_max_batch_size` for reads, `_deadline` per RPC, and
`_max_in_flight_read_requests` to shed reads to local evaluation rather than queue them. The last
two read options are backported from mainline and keep its names and defaults. Shed reads are
counted and reported as a warning at the end of the build, because they are served as misses and
nothing else distinguishes them from a cold cache.

## What the patch concedes

Two assumptions in Bazel's analysis caching do not hold outside Google, and the patch works around
them rather than solving them:

- **Absolute symlinks.** `FileDependencySerializer` rejects them outright, but an open-source output
  base is built from them — every repository served from the repository contents cache is reached by
  one. They become unresolvable leaves, which loses the dependency edge on whatever they point at.
- **Invalidation.** `LongVersionGetter` answers when a path last changed; inside Google that comes
  from the version control snapshot. This implementation reports every path as unchanged, disabling
  invalidation entirely. Sound only when the uploading and downloading workspaces hold identical
  content, which is why the service refuses to start without `--accept-unsound-invalidation`.

## Rebasing it

**From 10.0 the series starts by restoring the endpoint flag.** Upstream moved
`--experimental_remote_analysis_cache` into `BazelRulesModule`'s graveyard as a no-op in
`db6088a6e07`; the first patch reverts that, declaring the flag in `RemoteAnalysisCachingOptions`
again. The batch, deadline and in-flight flags come from `RemoteAnalysisCachingServicesOptions`,
which upstream declares but registers for no command, so the module registers it. Between 9.x and
10.0 `configure`, `lookup`, `Stats`, `WriteStatus` and the supplier interface all changed shape.

`master` is not a drop-in target. As of `d918229c048` the patch ports but the resulting binary
cannot extract an archive containing a symlink that escapes its directory, which some rulesets'
release tarballs do. Track releases, not master, until the hook is upstream.
