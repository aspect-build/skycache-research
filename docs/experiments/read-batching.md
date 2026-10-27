# Read batching

`--experimental_remote_analysis_cache_get_linger` decides read batch size in practice; the batch
size cap does not. Sweep it on a cold worktree, re-uploading before each leg.

**Bazel's own counter cannot measure this.** It reports logical lookups, so it reads the same
whether the client batched or not. Server-side proxies mislead too — a delta on stored rows is
contaminated by the upload, and rows-per-RPC conflates batch size with hit rate. The only honest
source is the service's own `keys=` log line, one per Get.

| linger | wall | Get RPCs | mean keys |
|-------:|-----:|---------:|----------:|
| 1 ms | 6.7s | 293 | 17.0 |
| 5 ms | 7.1s | 127 | 39.3 |
| **25 ms** | **6.5s** | **32** | **155.9** |
| 100 ms | 7.5s | 27 | 184.7 |
