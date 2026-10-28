# WatchKit Roadmap

## v0.1.0 (current)
The folder change and cache coordinator behind a C ABI: a portable polling snapshot-diff engine (size + mtime, no content hashing), a sequenced pending-event queue with the full coalescing table (net-zero create+delete, deleted+recreated = modified, rename chains collapsed), a bounded queue with counted — never silent — overflow drops, ignore globs with directory pruning, and a shared LRU cache for folder sizes and substring searches that the event stream invalidates at scan time. `wa_scan_once` drives scans synchronously; `wa_close` joins the polling thread.

## Next, in order
1. **Native change backend (Windows `ReadDirectoryChangesW`)**: feed OS change notifications into the same snapshot/queue/cache pipeline so large trees stop paying a full walk per interval and latency drops below one poll interval. The coalescing table, queue bounds, and invalidation rules are frozen contracts — the backend replaces only how raw changes are discovered, and polling stays as the portable fallback (and the test driver).
2. **Thumbnail-cache payload**: a third cache value kind holding opaque preview blobs keyed by path + size + mtime, so a host explorer's thumbnail pipeline uses the same invalidation and LRU bounds as sizes and searches instead of growing a parallel cache.
3. **Overflow recovery hint**: when `dropped > 0`, consumers currently learn *that* events were lost but not *where*; add a per-root "resync recommended" marker to status so a host knows which tree to rescan rather than guessing.
4. **Per-root pause/resume**: suspend scanning one root (a removable drive that went away) without tearing down the watcher or losing other roots' sequence continuity.

## Explicitly not planned
- Content hashing for change detection: it would make every scan read every byte of every watched file. Size + mtime, with its limit documented, is the honest trade at this layer; hosts that need certainty can hash the specific files events point at.
- Filesystem-event delivery via callbacks into host code from the polling thread: polling `wa_poll_events` is the baseline contract (a callback would run under scan timing and invite re-entrancy into the handle); hosts that want push can poll on their own thread.
- Watching individual files as roots: roots are directories by contract; a single-file watch is one root plus one ignore-everything-else pattern away, and a special case would complicate the snapshot model for no new capability.
