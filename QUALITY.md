# WatchKit — Quality Scorecard

Scored under The Elite Software Master Prompt v1.1, §17. Every score names its evidence; a score without evidence would be a Fail. This file is updated in the same change as the work it scores.

**State at v0.1.0: total 85.4/100 — the release bar (total ≥ 85, no dimension below 3, no Fail) is met for the local gate.** Two recorded waivers remain (Performance and Scalability measurements), and Windows runtime behaviour is Partially Verified: CI's real MSVC build is the link gate, and an on-machine field pass has not yet run.

| Dimension | Score (0–5) | Verdict | Evidence |
|---|---|---|---|
| Correctness (weight 14) | 4 | Pass | Full WinDevKit gate green locally (fmt, clippy both targets with `-D warnings`, all 31 tests): detection, the whole coalescing table, queue-overflow accounting, cache invalidation — each with a named deterministic test (see `docs/PARITY.md`). Not 5: behaviour on a real Windows machine is Unverified until a field pass; the VM gate is Linux evidence. |
| Security & privacy (12) | 5 | Pass | Zero dependencies; no network, no telemetry, no data leaves the watched folders' path names in caller-owned JSON; config parser rejects duplicate keys and caps nesting depth (family JSON engine); every retained structure is bounded (queue, cache) so a hostile filesystem cannot grow memory without limit; no secrets handled anywhere. |
| Robustness (11) | 4 | Pass | Unreadable entries and roots are skipped per scan instead of failing the watcher; locks recover from poisoning; every FFI entry point is `catch_unwind`-guarded; the cache epoch guard returns raced results uncached rather than storing stale data; queue overflow degrades to counted drops, never a wedge. Not 5: the formal failure-drill set (root deleted mid-run, permission churn) has not been run as a set. |
| Feature completeness (9) | 5 | Pass | Every v0.1 row in `docs/PARITY.md` is Complete with a named test; the one Not-started row (native backend) is a roadmap item outside the v0.1 claim, deferred with reason there. |
| Performance (8) | 3 | Waiver | Bounds are engineered (bounded queue/cache, sorted deterministic walks, no content reads), and scan cost is honestly documented as O(watched tree) per scan. Waiver: no benchmark has been measured yet — no scan-time figure is claimed anywhere; follow-up — record a large-tree scan benchmark before any performance claim is made or v0.2 ships. |
| Maintainability (7) | 4 | Pass | `AGENTS.md` records the load-bearing contracts (coalescing table, counted loss, scan-time invalidation, epoch guard, join-on-close) with the reasons; module split is core engine / JSON / FFI; header, README, ROADMAP and PARITY are current with the code. |
| Modularity (6) | 5 | Pass | Everything is an operation on the handle (`scan_once`, `poll_events`, `folder_size`, `cached_search`, `status`); the pure engine functions (glob, diff, merge, cache, walks) are independently unit-tested without the FFI; the FFI layer holds no logic beyond argument handling. |
| UX & accessibility (6) | 4 | Pass | For a C ABI the surface is the UX: one JSON config in, JSON results out, plain-language header documenting every export and every contract, honest error messages via the thread-local last-error. Not 5: no worked multi-file example program ships in the repo yet (README's C example is the guide). |
| Scalability (5) | 3 | Waiver | Queue and cache are hard-bounded and overflow behaviour is tested at the bound (`queue_overflow_drops_oldest_and_counts`, LRU eviction test). Waiver: no 10x/100x tree exercise has been run or recorded; follow-up — a synthetic large-tree scan/count check alongside the Performance benchmark. |
| Idiomatic quality (5) | 5 | Pass | `cargo fmt --check` clean and clippy `-D warnings` clean on host and `x86_64-pc-windows-msvc` targets in the local gate; zero dependencies; edition 2021 / rust-version 1.80 as per the family skeleton. |
| Test & corpus evidence (5) | 4 | Pass | 31 tests, deterministic by construction (manual scans; the single background-thread test waits on an outcome with a deadline, never a fixed sleep). Not 5: line-coverage percentage is not measured by tooling, so the 85%/70% floors are adopted targets, not verified facts. |
| Self-documentation (4) | 5 | Pass | Crate docs state the coalescing, loss, rename-heuristic, and directory-noise contracts where the code lives; `merge_one`, the cache, and the epoch guard carry the reasoning in comments at the point of decision. |
| Documentation (4) | 4 | Pass | README with TL;DR, C example, honest caveats (polling cost, size+mtime limit, rename heuristic, unreadable-root behaviour); header documents every export; ROADMAP separates planned from explicitly not planned. |
| Diagnostics & supportability (4) | 5 | Pass | `wa_status` exposes roots, files tracked, events emitted, queue length, dropped count, scan count, and full cache statistics — a host can explain any behaviour (including event loss) from one call; every failure path sets a specific last-error code and message. |

Weighted total: **85.4/100** (Correctness 11.2, Security 12.0, Robustness 8.8, Completeness 9.0, Performance 4.8, Maintainability 5.6, Modularity 6.0, UX 4.8, Scalability 3.0, Idiomatic 5.0, Test evidence 4.0, Self-documentation 4.0, Documentation 3.2, Diagnostics 4.0).

## What moves the score

1. On-machine Windows field pass (raises Correctness; anything found is fixed forward with a regression test).
2. A recorded large-tree scan benchmark (closes the Performance waiver and the Scalability follow-up in one exercise).
3. Coverage measurement, or recording test-count evidence as the accepted substitute (closes the Test evidence note).
