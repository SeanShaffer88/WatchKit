# Parity / Capability Matrix — WatchKit

Updated: 2026-10-28 · Overall readiness: v0.1.0 complete locally — full gate green (see QUALITY.md); Windows runtime evidence is CI's real MSVC build plus a future on-machine field pass (Partially Verified).

A row may say **Complete** only with a passing test or field result named in Evidence.

| Area | Target | State | Evidence (test / corpus) | Gap |
|---|---|---|---|---|
| Config | JSON config: roots (required), poll interval, ignore globs, queue/cache bounds; unknown + duplicate keys rejected | Complete | `config_parse_rules` (defaults, unknown/duplicate = parse error, type/range errors) + FFI error test | — |
| Baseline snapshot | Pre-existing files emit no events | Complete | `baseline_emits_no_events` | — |
| Detection | Created / modified / deleted via snapshot diff (size + mtime) | Complete | `create_modify_delete_detection` (incl. subdirectory events) | Same-size edit inside one mtime tick can be missed — documented limit, not a gap to close silently |
| Background polling | Thread polls at the configured interval; manual mode at interval 0 | Complete | `background_thread_picks_up_changes_and_close_joins` (bounded outcome wait) | — |
| Manual scan | `wa_scan_once` forces one synchronous scan; scans serialized | Complete | Every detection/coalescing test drives `scan_once` deterministically | — |
| Rename detection | Delete+create with same size+mtime pair as one `renamed` | Complete | `rename_pairing`; control case `unrelated_delete_create_same_scan_do_not_pair_when_sizes_differ` | Heuristic by design — limits documented in header/README |
| Coalescing | Full merge table incl. net-zero and rename chains; surviving event counts raw changes | Complete | `burst_create_plus_modifies_is_one_event_count_4`, `net_zero_create_then_delete_is_no_event`, `modified_then_deleted_is_one_deleted`, `deleted_then_recreated_is_one_modified`, `create_then_rename_is_one_created_at_new_name`, `rename_then_delete_is_deleted_of_original` | — |
| Poll semantics | Consuming poll; since_seq acknowledgement; max_events paging | Complete | `poll_since_seq_and_max_events`, `create_modify_delete_detection` (second poll empty) | — |
| Queue bound | Overflow drops oldest; `dropped` reported by poll and status | Complete | `queue_overflow_drops_oldest_and_counts` (exact seqs 7–10 survive of 10, dropped 6) | — |
| Ignore globs | `*` / `**` / `?`; name, path, and suffix matching; directory pruning | Complete | `glob_rules`, `ignore_globs_hide_files_and_prune_dirs` (later changes stay invisible) | — |
| Folder-size cache | Recursive totals cached; hit/miss counters | Complete | `folder_size_cache_hit_then_invalidation`, `folder_size_and_search_errors` | — |
| Cache invalidation | Change at/under (or above) a cached folder invalidates at scan time; unrelated changes do not | Complete | `folder_size_cache_hit_then_invalidation`, `search_cache_and_invalidation`, `folder_size_unrelated_change_keeps_cache` | — |
| Search cache | Case-insensitive substring matches, cached, invalidated like sizes | Complete | `search_cache_and_invalidation` | — |
| Cache bound | One shared LRU cache; `max_cache_entries` 0 disables | Complete | `cache_lru_eviction_at_bound`, `cache_zero_entries_disables_caching` | — |
| Multiple roots | Several recursive roots in one watcher | Complete | `multiple_roots` | — |
| Lifecycle | `wa_close` joins the polling thread; double close safe; Rust `Drop` also shuts down | Complete | `background_thread_picks_up_changes_and_close_joins` | — |
| FFI surface | Every header export exercised; null/malformed inputs return documented codes | Complete | `ffi_end_to_end`, `ffi_config_and_handle_errors` | Windows MSVC link verified by CI, not locally |
| Native change backend | `ReadDirectoryChangesW` feeding the same contracts | Not started | — | ROADMAP item 1; polling is the shipped v0.1 backend |
