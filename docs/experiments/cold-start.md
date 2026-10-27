# Cold start

Upload from a warm workspace, then download into a cold one. Uploads require a cold Skyframe —
`BuildView.UPLOAD_BUILDS_MUST_BE_COLD` is a hard error on a warm server, so `shutdown` first.

```
bazel shutdown
bazel build //app/... \
  --experimental_remote_analysis_cache_mode=UPLOAD \
  --experimental_remote_analysis_cache=grpc://$(minikube ip):30051 \
  --credential_helper="$(minikube ip)=<skycache>/experiments/local-credential-helper.sh" \
  --experimental_active_directories=app
```

**Suppress the clippy aspect on the download leg.** A workspace whose `.bazelrc` injects
`--aspects=…rust_clippy_aspect --output_groups=+clippy_checks` into every command hits this. The
aspect's `.clippy.ok` marker is an `ActionExecutionValue` the analysis cache serves back with a
*local* file proxy — a ctime, mtime and inode from the machine that uploaded it — so requesting that
output group in a fresh output base crashes with `Missing artifact … lib.clippy.ok`. Nothing can
materialise the file: it is a zero-byte marker, so it is not in any action cache either. Pass
`--output_groups=-clippy_checks` and the download completes:

```
Skycache stats: 16.42 MB received in 66562 requests, 6/6 cache hits (100.00%)
  [ACTION_EXECUTION: 2/2, ASPECT: 1/1, CONFIGURED_TARGET: 3/3]
```

This is the general shape of the limit rather than a clippy quirk: any requested output whose
cached execution value carries a local proxy has the same problem. Analysis-only consumers are
unaffected.

Then, in a *separate* worktree with its own output base, the same endpoint with
`--experimental_remote_analysis_cache_mode=DOWNLOAD` and no active directories.

**Wipe the output base between every leg, not once at the start.** Cleaning once up front
measures a warm second leg and reports a speed-up that is mostly the first leg's leftovers. Wiped
per leg, the figures are 18.8s against 126.9s, and 13 targets evaluated against 212,244.

`grpc://` matters: a bare `host:port` means TLS, which the dev manifests do not terminate.
