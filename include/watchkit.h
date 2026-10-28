/* WatchKit.dll — a folder change and cache coordinator:
 * coalesced change events and self-invalidating folder
 * caches behind one small C ABI.
 *
 *   WaWatcher *w;
 *   wa_create("{\"roots\":[\"C:\\\\data\"],"
 *             "\"poll_interval_ms\":250}", &w);
 *   // ... later, whenever convenient ...
 *   char *s; wa_poll_events(w, last_seq, 0, &s);
 *   printf("%s\n", s); wa_free_string(s);   // events JSON
 *   wa_close(w);                            // joins thread
 *
 * HOW IT WORKS IN v0.1. The engine POLLS: it snapshots
 * every root recursively (path -> size + mtime) and diffs
 * successive snapshots. That is deliberately portable —
 * the same code runs on every platform — and the
 * coalescing and cache contracts below are the product.
 * A ReadDirectoryChangesW backend is roadmap; it will
 * feed the same queue and cache, not replace them.
 * Change detection is size + mtime (content is never
 * hashed): an edit that keeps the exact same size inside
 * one mtime tick can be missed. Files that already exist
 * when the watcher is created are the BASELINE — they
 * emit no events.
 *
 * CONFIG (one JSON object):
 *
 *   {"roots": ["/data", "/more"], "poll_interval_ms": 250,
 *    "ignore": ["*.tmp", ".git/**"],
 *    "max_queue": 4096, "max_cache_entries": 256}
 *
 *   roots              (array of strings, required,
 *                      non-empty) directories watched
 *                      recursively. Each must exist and
 *                      be a directory at wa_create, or
 *                      the call fails WA_ERR_NOT_FOUND /
 *                      WA_ERR_INVALID_ARG.
 *   poll_interval_ms   (uint, default 500) background
 *                      scan interval. 0 DISables the
 *                      background thread: manual mode,
 *                      where scans happen only via
 *                      wa_scan_once (deterministic; what
 *                      tests use).
 *   ignore             (array of strings, default [])
 *                      glob patterns; matching files are
 *                      never tracked and matching folders
 *                      are pruned. Glob: "**" any chars
 *                      including '/', "*" any chars
 *                      except '/', "?" one char except
 *                      '/'. A pattern is tried against a
 *                      path's base name, its whole
 *                      root-relative path, and every
 *                      suffix of that path, so "*.tmp"
 *                      works at any depth and ".git/**"
 *                      prunes a .git folder at any depth.
 *   max_queue          (uint, default 4096, min 1) the
 *                      pending-event queue bound.
 *   max_cache_entries  (uint, default 256, min 0) the
 *                      shared size/search cache bound,
 *                      LRU-evicted. 0 disables caching.
 *
 * Unknown keys are an error (WA_ERR_PARSE), never
 * ignored; duplicate keys in one object are a parse
 * error too.
 *
 * EVENTS. wa_poll_events returns:
 *
 *   {"events":[{"seq":1,"type":"created",
 *     "path":"/data/a.txt","old_path":null,
 *     "coalesced_count":2}],
 *    "next_seq":1,"dropped":0,"queue_len":0}
 *
 *   type   created | modified | deleted | renamed.
 *          For renamed, "path" is the NEW path and
 *          "old_path" the previous one; otherwise
 *          old_path is null. Directories emit created
 *          and deleted only (a folder's mtime changes
 *          with every child change; the child events
 *          are the signal).
 *   seq    sequence numbers start at 1 and grow by one
 *          per DISTINCT event queued. Pass the returned
 *          next_seq back as since_seq on the next poll.
 *
 * COALESCING (the core contract). Changes to a path that
 * is still pending when a later change to the same path
 * arrives are MERGED into the pending event:
 *
 *   created  + modified -> one created
 *   created  + deleted  -> NO event (net-zero)
 *   modified + modified -> one modified
 *   modified + deleted  -> one deleted
 *   deleted  + created  -> one modified
 *   created  + renamed  -> one created at the new name
 *   renamed  + deleted  -> one deleted of the ORIGINAL
 *   renamed A->B + renamed B->C -> one renamed A->C
 *
 * The surviving event keeps the FIRST change's seq and
 * its coalesced_count counts every raw change it stands
 * for (created, then modified 3 times = one created
 * event with coalesced_count 4). Because a net-zero
 * removal retires a seq without delivering it, delivered
 * seqs can have gaps.
 *
 * RENAME DETECTION is a heuristic: within one scan, a
 * deleted FILE and a created file with identical size
 * and mtime pair as one renamed event (greedy, sorted
 * order, one-to-one). A genuine delete plus an unrelated
 * new file with the same size and mtime would pair the
 * same way; a rename that also edits content in the same
 * interval reads as deleted + created; a renamed
 * DIRECTORY surfaces as created + deleted for the folder
 * itself, with its children pairing individually when
 * their size+mtime survive.
 *
 * POLL SEMANTICS. wa_poll_events CONSUMES: returned
 * events leave the queue, and pending events with
 * seq <= since_seq are discarded as acknowledged (they
 * are never counted in "dropped"). max_events 0 means
 * no limit. Overflow drops the OLDEST pending events
 * and counts them in "dropped" — reported by every poll
 * and by wa_status — so loss is visible, never silent.
 *
 * CACHE COORDINATOR. wa_folder_size and wa_cached_search
 * share one LRU cache bounded by max_cache_entries:
 *
 *   wa_folder_size(w, path, &s) ->
 *     {"path":"...","total_bytes":N,"file_count":N,
 *      "dir_count":N,"cached":true|false}
 *     Recursive totals under path (symlinks count as
 *     files and are never followed). path must be an
 *     existing directory, else WA_ERR_NOT_FOUND /
 *     WA_ERR_INVALID_ARG.
 *
 *   wa_cached_search(w, root, substring, &s) ->
 *     {"root":"...","substring":"...",
 *      "matches":["...", ...],"count":N,"cached":bool}
 *     Every path at/under root whose full path contains
 *     substring, case-insensitively, sorted. Empty
 *     substring matches everything.
 *
 * Any scanned change at or under a cached folder — or
 * above it (a parent deleted or renamed) — invalidates
 * the entry at scan time, so a hit is never stale past
 * the next scan. wa_status reports cache_hits,
 * cache_misses, invalidations and cache_entries. This
 * pair stands in for the thumbnail/search caches of a
 * host explorer: compute once, serve hits, let the
 * event stream invalidate.
 *
 * STATUS. wa_status returns:
 *
 *   {"roots":[...],"poll_interval_ms":N,
 *    "files_tracked":N,"events_emitted":N,"queue_len":N,
 *    "dropped":N,"next_seq":N,"scans":N,
 *    "cache_entries":N,"cache_hits":N,"cache_misses":N,
 *    "invalidations":N}
 *
 *   files_tracked   entries in the current snapshot
 *                   (files + subfolders, roots excluded).
 *   events_emitted  distinct events ever queued (seqs
 *                   handed out); coalesced merges do not
 *                   add to it, net-zero removals do not
 *                   subtract.
 *   next_seq        the seq the next new event will get.
 *   scans           scans completed, background + manual.
 *
 * SCANS. One scan walks every root; unreadable entries
 * are skipped and an unreadable root reads as empty for
 * that scan. wa_scan_once runs one scan synchronously on
 * the caller's thread (background and manual scans are
 * serialized, so snapshots never apply out of order).
 *
 * wa_close(w) signals the background thread and JOINS
 * it before returning: a handle never leaks a thread.
 * NULL is a no-op. Anything through the handle after
 * wa_close is use-after-close.
 *
 * THREADING. The handle is thread-safe; every call is
 * serialized inside, and polls, status and cache queries
 * may come from any thread.
 *
 * All strings are UTF-8, NUL-terminated. char* results
 * are caller-owned: free with wa_free_string(). Errors
 * set a thread-local wa_last_error() (message + code).
 */
#ifndef WATCHKIT_H
#define WATCHKIT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(WA_BUILD)
#define WA_API __declspec(dllexport)
#elif defined(_WIN32)
#define WA_API __declspec(dllimport)
#else
#define WA_API
#endif

enum {
    WA_OK = 0,
    WA_ERR_INVALID_ARG = 1,  /* null pointers, missing/empty roots, wrong value types, not-a-directory */
    WA_ERR_NOT_SUPPORTED = 2,
    WA_ERR_NOT_FOUND = 3,    /* a root / folder path does not exist */
    WA_ERR_ACCESS_DENIED = 4,
    WA_ERR_PLATFORM = 5,     /* an OS/thread call failed; see wa_last_error() */
    WA_ERR_INTERNAL = 6,
    WA_ERR_PARSE = 7         /* config is not valid JSON, or has an unknown key */
};

typedef struct WaWatcher WaWatcher;

WA_API const char *wa_version(void);
WA_API const char *wa_last_error(void);
WA_API int wa_last_error_code(void);
WA_API void wa_free_string(char *p);

/* Create a watcher from a config JSON object (see
 * above). Builds the baseline snapshot synchronously
 * and starts the background polling thread unless
 * poll_interval_ms is 0. Returns WA_OK and a handle in
 * *out_watcher, or a WA_ERR_* code and *out_watcher is
 * NULL. */
WA_API int wa_create(const char *config_json, WaWatcher **out_watcher);

/* Run one scan synchronously (walk + diff + coalesce +
 * cache invalidation). Returns WA_OK. */
WA_API int wa_scan_once(WaWatcher *watcher);

/* Consume pending events with seq > since_seq, up to
 * max_events (0 = no limit), as one JSON object
 * (caller-owned; free with wa_free_string()). Shape
 * and merge/acknowledge rules: see above. */
WA_API int wa_poll_events(WaWatcher *watcher, uint64_t since_seq,
                          uint32_t max_events, char **out_json);

/* Recursive folder totals as one JSON object
 * (caller-owned), cached and self-invalidating. */
WA_API int wa_folder_size(WaWatcher *watcher, const char *path,
                          char **out_json);

/* Cached substring search under root as one JSON object
 * (caller-owned), invalidated exactly like folder
 * sizes. */
WA_API int wa_cached_search(WaWatcher *watcher, const char *root,
                            const char *substring, char **out_json);

/* Counters and cache statistics as one JSON object
 * (caller-owned). Shape: see above. */
WA_API int wa_status(WaWatcher *watcher, char **out_json);

/* Close and free the handle, JOINING the background
 * thread first. NULL is a no-op. */
WA_API void wa_close(WaWatcher *watcher);

#ifdef __cplusplus
}
#endif
#endif /* WATCHKIT_H */
