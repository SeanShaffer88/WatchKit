# WatchKit

## TL;DR

One watcher for every app, behind a tiny zero-dependency DLL: point it at one or more folders and it tells you what changed — created, modified, deleted, renamed — as a short, tidy list instead of a flood. If a file changes ten times before you ask, you hear about it once, with a count of ten. Folder sizes and searches are remembered, and the moment anything underneath them changes, the memory is thrown away and rebuilt fresh. Nothing is ever lost quietly: if changes pile up faster than you collect them, the count of what was dropped is reported, not hidden.

```c
#include "watchkit.h"

int main(void) {
    WaWatcher *w;
    wa_create("{\"roots\":[\"/data\"],\"poll_interval_ms\":250,"
              "\"ignore\":[\"*.tmp\",\".git/**\"]}", &w);
    uint64_t since = 0;
    for (;;) {
        char *s; wa_poll_events(w, since, 0, &s);
        /* {"events":[{"seq":1,"type":"created","path":...,
         *   "old_path":null,"coalesced_count":2}], ...} */
        printf("%s\n", s); wa_free_string(s);
        /* ... react, refresh folder sizes / searches ... */
    }
    wa_close(w);   /* joins the polling thread */
}
```

## Why this exists

Every file explorer, sync tool, and media app ends up needing the same two things: "tell me what changed in this folder" and "don't recompute the folder size / search results / thumbnails on every paint." Both are easy to sketch and surprisingly fiddly to get right — bursts of changes arrive faster than any consumer wants them, caches go stale in exactly the cases users notice, and a queue that silently drops events produces an explorer that is confidently wrong. WatchKit is those two jobs done once, with the rules written down:

- **Coalesced events.** Pending changes to the same path merge before you ever see them. A file created and then edited three times before your next poll arrives as *one* `created` event with `coalesced_count` 4. A file created and deleted inside the same window produces *no* event at all — nothing net happened. Modified then deleted is one `deleted`; deleted then recreated is one `modified`. The surviving event keeps the first change's sequence number and counts every raw change it stands for.
- **A bounded queue with counted loss.** The pending queue holds at most `max_queue` events (default 4096). Overflow drops the oldest — and increments a `dropped` counter that every poll and every status call reports. Loss is a number you can see, never a silent gap.
- **Self-invalidating caches.** `wa_folder_size` and `wa_cached_search` share one LRU cache (default 256 entries). Any scanned change at or under a cached folder — or above it, if a parent was deleted or renamed — invalidates the entry at scan time. A hit is therefore never stale past the next scan, and you never write invalidation code in the app. This pair stands in for the thumbnail and search caches of a host explorer: compute once, serve hits, let the event stream invalidate.
- **Honest change detection.** Snapshots store size + modification time per path — content is never hashed, so detection is cheap and the failure mode is documented instead of hidden (see caveats).

## How v0.1 works: polling, plainly

The engine **polls**: a background thread snapshots every root recursively and diffs successive snapshots (`poll_interval_ms`, default 500). That makes the engine identical on every platform and — just as important — deterministic: `wa_scan_once` runs one scan synchronously, and setting `poll_interval_ms` to 0 turns the background thread off entirely, which is how the test suite drives every scenario without sleeping on timers.

The coalescing, queue, and cache contracts are the product; the polling backend is an implementation detail with a known cost (each scan walks the watched trees) and a known ceiling (changes between two scans that cancel out before either is seen — a file created *and* deleted entirely inside one poll interval — never appear, because no snapshot ever contained the file; the net-zero rule only covers changes a scan actually observed). A native `ReadDirectoryChangesW` backend is on the roadmap and will feed the same queue and cache unchanged.

Rename detection is a documented heuristic: within one scan, a deleted file and a created file with identical size and mtime pair as one `renamed` event (greedy, sorted order, one-to-one). Directories emit only `created`/`deleted` — a folder's mtime changes with every child change, and reporting it would add a noise event for every real one.

## The API in one screen

| Area | Calls |
|---|---|
| Lifecycle | `wa_create` (config JSON), `wa_close` (signals + joins the polling thread) |
| Scanning | `wa_scan_once` (one synchronous scan) |
| Events | `wa_poll_events` (since_seq, max_events → `{events, next_seq, dropped, queue_len}`) |
| Cache | `wa_folder_size` (recursive totals), `wa_cached_search` (case-insensitive substring) |
| Status | `wa_status` → roots, files_tracked, events_emitted, queue_len, dropped, scans, cache hits/misses/invalidations |
| Errors | Integer `WA_*` codes + thread-local `wa_last_error()` |

Config: `{"roots":[...], "poll_interval_ms":N, "ignore":["*.tmp",".git/**"], "max_queue":4096, "max_cache_entries":256}`. Only `roots` is required; each root must be an existing directory. Globs: `**` crosses `/`, `*` and `?` do not; a pattern is tried against the base name, the root-relative path, and every suffix of it, so `*.tmp` and `.git/**` both work at any depth. `wa_poll_events` consumes — returned events leave the queue, and `seq <= since_seq` counts as acknowledged — so pass each poll's `next_seq` into the next call.

## Honest caveats

- **Polling latency and cost.** A change is reported at the next scan, not the instant it happens, and every scan walks the watched trees. For very large trees or instant notification, the roadmap's native backend is the answer; v0.1 trades immediacy for portability and determinism.
- **Size + mtime is the signal.** An edit that preserves the exact byte size *and* lands inside one mtime tick can be missed. Editors that rewrite a file in place with the same length at coarse timestamp granularity are the realistic case.
- **Renames are paired, not proven.** A real delete plus an unrelated new file with identical size and mtime pairs as a rename; a rename that also edits content in the same interval reads as deleted + created; a renamed directory surfaces as created + deleted for the folder, with its children pairing individually when their size+mtime survive.
- **An unreadable root reads as empty for that scan** (for example, a network share that drops out will surface as a burst of deletions, and its return as creations). Watch network roots with that behaviour in mind.
- **Symlinks are never followed**, so link cycles cannot loop a scan; a symlink is tracked as a file-sized entry.

## Verification status

31 tests, all driven through `wa_scan_once` except one bounded background-thread wait: detection (create/modify/delete, baseline silence, subdirectory events without directory-modified noise), rename pairing and its non-pairing control case, the coalescing table (burst create + 3 modifies = one event with count 4, net-zero create+delete, modified→deleted, deleted→recreated, created→renamed, renamed→deleted of the original), poll semantics (since_seq acknowledgement, max_events paging, consumption), queue overflow dropping the oldest with exact accounting, ignore globs (files hidden, `.git` pruned), folder-size cache hit then invalidation, an unrelated change provably *keeping* a cache entry, search cache invalidation, LRU eviction at the bound, cache-disabled mode, error codes, multiple roots, background pickup with a clean double close, and FFI end-to-end JSON field checks. Windows CI runs the full WinDevKit gate — format, clippy on both targets, the whole suite, and a real MSVC build and link. Windows runtime behaviour beyond CI is Unverified until an on-machine pass.

Building: `cargo build --release` → `watchkit.dll` / `libwatchkit.so`; `cargo test --locked`.

License: MIT. Part of the WinDevKit family — see `AGENTS.md` for build discipline and `ROADMAP.md` for what's next.
