//! WatchKit — a folder change and cache coordinator behind
//! a small C ABI.
//!
//! One handle watches one or more recursive roots. A
//! polling snapshot-diff engine (the portable v0.1 core)
//! compares successive snapshots — `path -> {size, mtime,
//! is_dir}` — and turns differences into a bounded,
//! sequenced, *coalesced* event queue. On top of the same
//! event stream sits a cache coordinator: folder sizes and
//! search results are cached, and any change at or under a
//! cached folder invalidates that entry (and is never
//! silently lost: queue overflow is counted, not hidden).
//!
//! Contracts that are load-bearing, not decoration:
//!
//! - **Coalescing.** Events still pending when a later
//!   change to the same path arrives are merged: created
//!   then modified stays one `created`; created then
//!   deleted cancels out entirely (net-zero: no event);
//!   modified then deleted becomes one `deleted`; deleted
//!   then recreated becomes one `modified`. The surviving
//!   event's `coalesced_count` says how many raw changes
//!   it stands for.
//! - **Bounded queue, counted loss.** The pending queue
//!   holds at most `max_queue` events; overflow drops the
//!   oldest and increments a `dropped` counter that every
//!   poll and status reports. Loss is never silent.
//! - **Rename heuristic.** A delete and a create in the
//!   same scan with identical size and mtime are paired
//!   as one `renamed` event. That is a heuristic: a real
//!   delete plus an unrelated new file with the same size
//!   and mtime would pair the same way, and a rename that
//!   also changes content in the same interval reads as
//!   delete + create. Documented in the header.
//! - **Directories** emit created/deleted only. A
//!   directory's mtime changes whenever a child comes or
//!   goes; reporting it as `modified` would add a noise
//!   event for every real child event. Child changes are
//!   the events.
//! - **No leaked thread.** `wa_close` signals the polling
//!   thread and joins it before returning.
//!
//! The portable core is pure `std::fs` polling —
//! deterministic (`wa_scan_once` drives it synchronously
//! in tests). Since v0.2 a native `ReadDirectoryChangesW`
//! backend (Windows, `src/platform.rs`) can discover raw
//! changes instead; it feeds the SAME snapshot/queue/cache
//! pipeline through `apply_observed_changes`, so the
//! coalescing and cache contracts above are the product
//! and do not change under it. Polling stays the fallback
//! (and what runs everywhere else); `wa_status` reports
//! which backend is actually running.

pub mod ffi;
pub mod json;
pub mod platform;

pub const VERSION: &str = "0.2.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 2;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/watchkit.h).
pub const WA_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_SUPPORTED: i32 = 2;
pub const ERR_NOT_FOUND: i32 = 3;
pub const ERR_ACCESS_DENIED: i32 = 4;
pub const ERR_PLATFORM: i32 = 5;
pub const ERR_INTERNAL: i32 = 6;
pub const ERR_PARSE: i32 = 7;

pub type WaResult<T> = Result<T, (i32, String)>;

/// Escape a string for inclusion inside JSON double quotes.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned lock means a panic happened while holding
    // it; the watcher state is still structurally valid,
    // so recover the guard rather than wedging every call.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

pub const DEFAULT_POLL_INTERVAL_MS: u64 = 500;
pub const DEFAULT_MAX_QUEUE: usize = 4096;
pub const DEFAULT_MAX_CACHE_ENTRIES: usize = 256;

/// Which change-discovery backend a config asks for.
/// The choice is a *request*: `wa_status` reports the
/// backend that is actually running, and a native request
/// that cannot start falls back to polling (and says so).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Native where available (Windows), polling elsewhere.
    Auto,
    /// Native (`ReadDirectoryChangesW`) — Windows only;
    /// falls back to polling when it cannot start.
    Native,
    /// The portable polling snapshot-diff engine.
    Polling,
}

/// The backend a watcher is actually running. Reported by
/// `wa_status` as `"native"` or `"polling"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunningBackend {
    Native,
    Polling,
}

impl RunningBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunningBackend::Native => "native",
            RunningBackend::Polling => "polling",
        }
    }
}

/// A fully-validated watcher configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Recursive roots (directories). Required, non-empty.
    pub roots: Vec<String>,
    /// Background poll interval. 0 disables the background
    /// thread (manual mode: scans happen only via
    /// `scan_once` — the deterministic test configuration).
    /// Manual mode also means the polling driver is what
    /// runs, whatever `backend` says: there is no
    /// background discovery to be native.
    pub poll_interval_ms: u64,
    /// Requested change-discovery backend (default Auto).
    pub backend: Backend,
    /// Ignore glob patterns (see `glob_match`).
    pub ignore: Vec<String>,
    /// Pending-event queue bound (>= 1).
    pub max_queue: usize,
    /// Cache bound in entries (0 disables caching).
    pub max_cache_entries: usize,
}

/// Parse and validate a `wa_create` config JSON object:
///
///   {"roots": ["/data"], "poll_interval_ms": 250,
///    "backend": "auto",
///    "ignore": ["*.tmp", ".git/**"],
///    "max_queue": 4096, "max_cache_entries": 256}
///
/// Only `roots` is required. Unknown keys and duplicate
/// keys are `ERR_PARSE`; missing/wrong-typed/out-of-range
/// values are `ERR_INVALID_ARG` (family convention).
pub fn parse_config(text: &str) -> WaResult<Config> {
    let value =
        json::parse(text).map_err(|e| (ERR_PARSE, format!("config is not valid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or((ERR_PARSE, "config must be a JSON object".to_string()))?;
    for (key, _) in obj {
        if !matches!(
            key.as_str(),
            "roots" | "poll_interval_ms" | "backend" | "ignore" | "max_queue" | "max_cache_entries"
        ) {
            return Err((ERR_PARSE, format!("unknown config key \"{key}\"")));
        }
    }
    let roots = match value.get("roots") {
        None => {
            return Err((
                ERR_INVALID_ARG,
                "config is missing required key \"roots\"".to_string(),
            ))
        }
        Some(json::Value::Array(items)) => {
            if items.is_empty() {
                return Err((
                    ERR_INVALID_ARG,
                    "\"roots\" must contain at least one root".to_string(),
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    json::Value::Str(s) if !s.is_empty() => out.push(s.clone()),
                    _ => {
                        return Err((
                            ERR_INVALID_ARG,
                            "\"roots\" entries must be non-empty strings".to_string(),
                        ))
                    }
                }
            }
            out
        }
        Some(other) => {
            return Err((
                ERR_INVALID_ARG,
                format!(
                    "\"roots\" must be an array of strings, got {}",
                    other.type_name()
                ),
            ))
        }
    };
    let ignore = match value.get("ignore") {
        None => Vec::new(),
        Some(json::Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    json::Value::Str(s) => out.push(s.clone()),
                    _ => {
                        return Err((
                            ERR_INVALID_ARG,
                            "\"ignore\" entries must be strings".to_string(),
                        ))
                    }
                }
            }
            out
        }
        Some(other) => {
            return Err((
                ERR_INVALID_ARG,
                format!(
                    "\"ignore\" must be an array of strings, got {}",
                    other.type_name()
                ),
            ))
        }
    };
    let poll_interval_ms = parse_u64_opt(&value, "poll_interval_ms", DEFAULT_POLL_INTERVAL_MS)?;
    let backend = match value.get("backend") {
        None => Backend::Auto,
        Some(json::Value::Str(s)) => match s.as_str() {
            "auto" => Backend::Auto,
            "native" => Backend::Native,
            "polling" => Backend::Polling,
            other => {
                return Err((
                    ERR_INVALID_ARG,
                    format!(
                        "\"backend\" must be \"auto\", \"native\" or \"polling\", got \"{other}\""
                    ),
                ))
            }
        },
        Some(other) => {
            return Err((
                ERR_INVALID_ARG,
                format!("\"backend\" must be a string, got {}", other.type_name()),
            ))
        }
    };
    let max_queue = parse_usize_opt(&value, "max_queue", DEFAULT_MAX_QUEUE, 1)?;
    let max_cache_entries =
        parse_usize_opt(&value, "max_cache_entries", DEFAULT_MAX_CACHE_ENTRIES, 0)?;
    Ok(Config {
        roots,
        poll_interval_ms,
        backend,
        ignore,
        max_queue,
        max_cache_entries,
    })
}

fn parse_u64_opt(value: &json::Value, key: &str, default: u64) -> WaResult<u64> {
    match value.get(key) {
        None => Ok(default),
        Some(v) => match v.as_i64() {
            Some(n) if n >= 0 => Ok(n as u64),
            _ => Err((
                ERR_INVALID_ARG,
                format!("\"{key}\" must be a non-negative integer"),
            )),
        },
    }
}

fn parse_usize_opt(value: &json::Value, key: &str, default: usize, min: usize) -> WaResult<usize> {
    let n = parse_u64_opt(value, key, default as u64)?;
    if (n as usize) < min {
        return Err((ERR_INVALID_ARG, format!("\"{key}\" must be at least {min}")));
    }
    Ok(n as usize)
}

// ---------------------------------------------------------------------------
// Glob matching (ignore patterns)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum GlobTok {
    /// `**` — any run of characters, including `/`.
    AnyAll,
    /// `*` — any run of characters except `/`.
    AnySeq,
    /// `?` — exactly one character except `/`.
    AnyOne,
    Lit(char),
}

fn glob_tokens(pattern: &str) -> Vec<GlobTok> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut toks = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                toks.push(GlobTok::AnyAll);
                i += 2;
            }
            '*' => {
                toks.push(GlobTok::AnySeq);
                i += 1;
            }
            '?' => {
                toks.push(GlobTok::AnyOne);
                i += 1;
            }
            c => {
                toks.push(GlobTok::Lit(c));
                i += 1;
            }
        }
    }
    toks
}

/// Match `text` against a glob `pattern`. Syntax: `**`
/// matches any characters including `/`; `*` matches any
/// characters except `/`; `?` matches one character
/// except `/`; everything else is literal. Matching is
/// case-sensitive and anchored at both ends.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let toks = glob_tokens(pattern);
    let text: Vec<char> = text.chars().collect();
    // DP over (token index, text index); sizes are tiny
    // (patterns and path segments), clarity wins.
    let mut dp = vec![vec![false; text.len() + 1]; toks.len() + 1];
    dp[toks.len()][text.len()] = true;
    for ti in (0..toks.len()).rev() {
        for xi in (0..=text.len()).rev() {
            dp[ti][xi] = match toks[ti] {
                GlobTok::Lit(c) => xi < text.len() && text[xi] == c && dp[ti + 1][xi + 1],
                GlobTok::AnyOne => xi < text.len() && text[xi] != '/' && dp[ti + 1][xi + 1],
                GlobTok::AnySeq => {
                    dp[ti + 1][xi] || (xi < text.len() && text[xi] != '/' && dp[ti][xi + 1])
                }
                GlobTok::AnyAll => dp[ti + 1][xi] || (xi < text.len() && dp[ti][xi + 1]),
            };
        }
    }
    dp[0][0]
}

/// Is `rel_path` (relative to a root, `/`-separated)
/// matched by any ignore pattern? A pattern is tried
/// against the base name, the whole relative path, and
/// every suffix of the relative path, so both `*.tmp`
/// (any depth) and `.git/**` (a `.git` folder at any
/// depth) behave the way a developer expects.
pub fn is_ignored(rel_path: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let base = rel_path.rsplit('/').next().unwrap_or(rel_path);
    for pat in patterns {
        if glob_match(pat, base) || glob_match(pat, rel_path) {
            return true;
        }
        // Suffixes: ".git/x/y" inside "repo/.git/x/y".
        let mut rest = rel_path;
        while let Some(idx) = rest.find('/') {
            rest = &rest[idx + 1..];
            if glob_match(pat, rest) {
                return true;
            }
        }
        // "dir/**" also matches "dir" itself.
        if let Some(prefix) = pat.strip_suffix("/**") {
            if rel_path == prefix || base == prefix {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Events and the coalescing queue
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Created => "created",
            EventKind::Modified => "modified",
            EventKind::Deleted => "deleted",
            EventKind::Renamed => "renamed",
        }
    }
}

/// A raw change from one snapshot diff, before queue
/// merging assigns it a sequence number.
#[derive(Clone, Debug, PartialEq)]
pub struct RawEvent {
    pub kind: EventKind,
    /// The path the event concerns. For `Renamed` this is
    /// the NEW path; `old_path` holds the previous one.
    pub path: String,
    pub old_path: Option<String>,
}

impl RawEvent {
    fn new(kind: EventKind, path: String) -> Self {
        RawEvent {
            kind,
            path,
            old_path: None,
        }
    }
}

/// A queued event: sequence-numbered and coalesced.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub seq: u64,
    pub kind: EventKind,
    pub path: String,
    pub old_path: Option<String>,
    /// How many raw changes this event stands for (>= 1).
    pub coalesced_count: u64,
}

impl Event {
    pub fn to_json(&self) -> String {
        let old = match &self.old_path {
            Some(p) => format!("\"{}\"", json_escape(p)),
            None => "null".to_string(),
        };
        format!(
            "{{\"seq\":{},\"type\":\"{}\",\"path\":\"{}\",\"old_path\":{},\"coalesced_count\":{}}}",
            self.seq,
            self.kind.as_str(),
            json_escape(&self.path),
            old,
            self.coalesced_count
        )
    }
}

/// Merge one raw event into the pending queue (`pending`
/// is ordered by seq). This function IS the coalescing
/// contract; see the crate docs. Returns true when the
/// raw event was absorbed without a new queue entry
/// (merged or net-zero-cancelled).
fn merge_one(
    pending: &mut std::collections::VecDeque<Event>,
    raw: &RawEvent,
    next_seq: &mut u64,
    events_emitted: &mut u64,
    max_queue: usize,
    dropped: &mut u64,
) {
    let push_new = |pending: &mut std::collections::VecDeque<Event>,
                    next_seq: &mut u64,
                    events_emitted: &mut u64,
                    dropped: &mut u64| {
        let seq = *next_seq;
        *next_seq += 1;
        *events_emitted += 1;
        pending.push_back(Event {
            seq,
            kind: raw.kind,
            path: raw.path.clone(),
            old_path: raw.old_path.clone(),
            coalesced_count: 1,
        });
        while pending.len() > max_queue {
            pending.pop_front();
            *dropped += 1;
        }
    };

    match raw.kind {
        EventKind::Created => {
            if let Some(e) = pending.iter_mut().find(|e| e.path == raw.path) {
                if e.kind == EventKind::Deleted {
                    // Deleted then recreated before the
                    // consumer polled: net effect modified.
                    e.kind = EventKind::Modified;
                    e.old_path = None;
                }
                e.coalesced_count += 1;
            } else {
                push_new(pending, next_seq, events_emitted, dropped);
            }
        }
        EventKind::Modified => {
            if let Some(e) = pending.iter_mut().find(|e| e.path == raw.path) {
                if e.kind == EventKind::Deleted {
                    e.kind = EventKind::Modified;
                    e.old_path = None;
                }
                e.coalesced_count += 1;
            } else {
                push_new(pending, next_seq, events_emitted, dropped);
            }
        }
        EventKind::Deleted => {
            if let Some(pos) = pending.iter().position(|e| e.path == raw.path) {
                match pending[pos].kind {
                    EventKind::Created => {
                        // Net-zero: created and deleted
                        // entirely inside one pending
                        // window. The consumer never hears
                        // about the path at all.
                        pending.remove(pos);
                    }
                    EventKind::Renamed => {
                        // Renamed here, then deleted: the
                        // net effect is the ORIGINAL path
                        // was deleted.
                        let old = pending[pos].old_path.take();
                        pending[pos].path = old.unwrap_or_else(|| raw.path.clone());
                        pending[pos].kind = EventKind::Deleted;
                        pending[pos].coalesced_count += 1;
                    }
                    EventKind::Modified | EventKind::Deleted => {
                        pending[pos].kind = EventKind::Deleted;
                        pending[pos].old_path = None;
                        pending[pos].coalesced_count += 1;
                    }
                }
            } else {
                push_new(pending, next_seq, events_emitted, dropped);
            }
        }
        EventKind::Renamed => {
            let old = raw.old_path.clone().unwrap_or_default();
            let src_pos = pending.iter().position(|e| e.path == old);
            match src_pos {
                Some(pos) => match pending[pos].kind {
                    EventKind::Created => {
                        // Created under the old name, then
                        // renamed before the poll: still
                        // just a creation, at the new name.
                        pending[pos].path = raw.path.clone();
                        pending[pos].coalesced_count += 1;
                    }
                    EventKind::Modified => {
                        pending[pos].kind = EventKind::Renamed;
                        pending[pos].old_path = Some(old);
                        pending[pos].path = raw.path.clone();
                        pending[pos].coalesced_count += 1;
                    }
                    EventKind::Renamed => {
                        // A -> B -> C collapses to A -> C.
                        pending[pos].path = raw.path.clone();
                        pending[pos].coalesced_count += 1;
                    }
                    EventKind::Deleted => {
                        push_new(pending, next_seq, events_emitted, dropped);
                    }
                },
                None => push_new(pending, next_seq, events_emitted, dropped),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cache coordinator
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum CacheValue {
    FolderSize {
        total_bytes: u64,
        file_count: u64,
        dir_count: u64,
    },
    Search {
        matches: Vec<String>,
    },
}

#[derive(Clone, Debug)]
struct CacheEntry {
    key: String,
    /// The folder the entry describes; events at/under
    /// it (or above it) invalidate the entry.
    anchor: String,
    value: CacheValue,
    last_used: u64,
}

/// One bounded LRU cache shared by folder sizes and
/// searches. Eviction is by least-recent use.
#[derive(Debug, Default)]
pub struct Cache {
    entries: Vec<CacheEntry>,
    tick: u64,
}

impl Cache {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&mut self, key: &str) -> Option<CacheValue> {
        let pos = self.entries.iter().position(|e| e.key == key)?;
        self.tick += 1;
        self.entries[pos].last_used = self.tick;
        Some(self.entries[pos].value.clone())
    }

    pub fn insert(&mut self, key: String, anchor: String, value: CacheValue, max: usize) {
        if max == 0 {
            return;
        }
        if let Some(e) = self.entries.iter_mut().find(|e| e.key == key) {
            self.tick += 1;
            e.value = value;
            e.anchor = anchor;
            e.last_used = self.tick;
            return;
        }
        while self.entries.len() >= max {
            // Evict least-recently used.
            let lru = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(i, _)| i)
                .expect("cache non-empty while over bound");
            self.entries.remove(lru);
        }
        self.tick += 1;
        self.entries.push(CacheEntry {
            key,
            anchor,
            value,
            last_used: self.tick,
        });
    }

    /// Remove every entry affected by a change at `path`
    /// (or, for a rename, at `old_path` too). An entry is
    /// affected when the change is at/under its anchor,
    /// or its anchor is at/under the changed path (a
    /// parent folder was deleted or renamed away).
    /// Returns how many entries were removed.
    pub fn invalidate_for(&mut self, path: &str, old_path: Option<&str>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| {
            !at_or_under(path, &e.anchor)
                && !at_or_under(&e.anchor, path)
                && !old_path.is_some_and(|o| at_or_under(o, &e.anchor) || at_or_under(&e.anchor, o))
        });
        before - self.entries.len()
    }
}

/// Component-wise "path is the anchor or lives under it".
pub fn at_or_under(path: &str, anchor: &str) -> bool {
    std::path::Path::new(path).starts_with(std::path::Path::new(anchor))
}

/// Trim trailing separators (keeping a bare root) so the
/// same folder named two ways shares one cache entry.
pub fn normalize_path_str(p: &str) -> String {
    let trimmed = p.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        p.to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Snapshots and diffing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub size: u64,
    /// Modification time, nanoseconds since the Unix
    /// epoch (0 when the platform reports none). Content
    /// is NOT hashed: size + mtime is the documented
    /// change signal, so a same-size edit inside one
    /// mtime tick can be missed — see the README.
    pub mtime_nanos: u128,
    pub is_dir: bool,
}

pub type Snapshot = std::collections::BTreeMap<String, SnapshotEntry>;

pub(crate) fn mtime_nanos_of(md: &std::fs::Metadata) -> u128 {
    match md.modified() {
        Ok(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// Build a snapshot of every root. Unreadable entries are
/// skipped; an unreadable root contributes nothing for
/// this scan (documented: it reads as empty).
pub fn build_snapshot(config: &Config) -> Snapshot {
    build_snapshot_of(config, &config.roots)
}

/// Build a snapshot of the given roots only (same rules
/// as `build_snapshot`). The native backend's overflow
/// rescan uses this to redo one root without walking the
/// others.
pub fn build_snapshot_of(config: &Config, roots: &[String]) -> Snapshot {
    let mut map = Snapshot::new();
    for root in roots {
        let root_path = std::path::Path::new(root);
        collect(root_path, root_path, &config.ignore, &mut map);
    }
    map
}

fn collect(dir: &std::path::Path, root: &std::path::Path, ignore: &[String], map: &mut Snapshot) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut entries: Vec<std::path::PathBuf> =
        read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        let rel = match path.strip_prefix(root) {
            Ok(r) => r
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => continue,
        };
        if is_ignored(&rel, ignore) {
            continue; // ignored dirs are pruned, not entered
        }
        let md = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ft = md.file_type();
        let key = path.to_string_lossy().into_owned();
        if ft.is_dir() {
            map.insert(
                key,
                SnapshotEntry {
                    size: 0,
                    mtime_nanos: mtime_nanos_of(&md),
                    is_dir: true,
                },
            );
            collect(&path, root, ignore, map);
        } else {
            // Files and symlinks (symlinks are never
            // followed, so a link cycle cannot loop).
            map.insert(
                key,
                SnapshotEntry {
                    size: md.len(),
                    mtime_nanos: mtime_nanos_of(&md),
                    is_dir: false,
                },
            );
        }
    }
}

/// Diff two snapshots into raw events. Rename pairing:
/// a deleted file and a created file in the same diff
/// with identical size and mtime pair as one rename
/// (greedy, in sorted path order, one-to-one, files
/// only). Raw events come out sorted by path (a rename
/// sorts by its new path) so scans are deterministic.
pub fn diff_snapshots(old: &Snapshot, new: &Snapshot) -> Vec<RawEvent> {
    let mut raws: Vec<RawEvent> = Vec::new();
    let mut deleted: Vec<(&String, &SnapshotEntry)> = Vec::new();
    let mut created: Vec<(&String, &SnapshotEntry)> = Vec::new();

    for (path, ne) in new {
        match old.get(path) {
            None => created.push((path, ne)),
            Some(oe) => {
                if oe.is_dir != ne.is_dir {
                    deleted.push((path, oe));
                    created.push((path, ne));
                } else if !ne.is_dir && (oe.size != ne.size || oe.mtime_nanos != ne.mtime_nanos) {
                    raws.push(RawEvent::new(EventKind::Modified, path.clone()));
                }
            }
        }
    }
    for (path, oe) in old {
        if !new.contains_key(path) {
            deleted.push((path, oe));
        }
    }

    // Pair deletes with creates as renames (files only).
    let mut used_created: Vec<bool> = vec![false; created.len()];
    let mut paired_deleted: Vec<bool> = vec![false; deleted.len()];
    for (di, (dpath, de)) in deleted.iter().enumerate() {
        if de.is_dir {
            continue;
        }
        for (ci, (cpath, ce)) in created.iter().enumerate() {
            if used_created[ci] || ce.is_dir {
                continue;
            }
            if ce.size == de.size && ce.mtime_nanos == de.mtime_nanos && cpath != dpath {
                used_created[ci] = true;
                paired_deleted[di] = true;
                raws.push(RawEvent {
                    kind: EventKind::Renamed,
                    path: (*cpath).clone(),
                    old_path: Some((*dpath).clone()),
                });
                break;
            }
        }
    }
    for (i, (path, _)) in created.iter().enumerate() {
        if !used_created[i] {
            raws.push(RawEvent::new(EventKind::Created, (*path).clone()));
        }
    }
    for (i, (path, _)) in deleted.iter().enumerate() {
        if !paired_deleted[i] {
            raws.push(RawEvent::new(EventKind::Deleted, (*path).clone()));
        }
    }
    raws.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
    });
    raws
}

/// Merge a batch of *observed* named changes into
/// `snapshot`, returning the raw events the batch stands
/// for. This is the convergence point between the polling
/// engine and the native backend: the caller (the
/// `ReadDirectoryChangesW` thread) stats each path the OS
/// named and reports `(full_path, Some(entry))` when it
/// exists now or `(full_path, None)` when it is gone —
/// which is exactly how the actions map: ADDED and
/// RENAMED_NEW_NAME stat to an entry (upsert), REMOVED and
/// RENAMED_OLD_NAME stat to nothing (removal), MODIFIED
/// stats to its fresh entry (upsert). Duplicate paths in
/// one batch fold last-wins before diffing.
///
/// The events come from running the ordinary snapshot
/// diff over just the affected subset, so rename pairing,
/// directory rules and event shapes are the poller's —
/// including pairing a RENAMED_OLD + RENAMED_NEW pair that
/// lands in one batch into a single `renamed` event.
/// Removing a directory also removes every snapshot entry
/// under it (each surfaces as its own `deleted`, exactly
/// as a polling diff of the same change would report).
/// Pure: no I/O, no locks — unit-tested on every platform.
pub fn merge_observed_into_snapshot(
    snapshot: &mut Snapshot,
    observed: &[(String, Option<SnapshotEntry>)],
) -> Vec<RawEvent> {
    // Fold duplicates: the last observation of a path is
    // the state the batch leaves it in.
    let mut folded: std::collections::BTreeMap<&str, Option<SnapshotEntry>> =
        std::collections::BTreeMap::new();
    for (path, entry) in observed {
        folded.insert(path.as_str(), *entry);
    }
    if folded.is_empty() {
        return Vec::new();
    }
    // The "before" picture: the folded paths themselves,
    // plus — when a directory disappears or turns into a
    // file — every entry that lived under it.
    let mut old_sub: Snapshot = Snapshot::new();
    for (path, new_entry) in &folded {
        if let Some(old) = snapshot.get(*path) {
            old_sub.insert((*path).to_string(), *old);
            let dir_vanishes = old.is_dir && new_entry.map(|e| !e.is_dir).unwrap_or(true);
            if dir_vanishes {
                for key in snapshot.keys() {
                    if key.as_str() != *path && at_or_under(key, path) {
                        old_sub.insert(key.clone(), snapshot[key]);
                    }
                }
            }
        }
    }
    let mut new_sub: Snapshot = Snapshot::new();
    for (path, entry) in &folded {
        if let Some(e) = entry {
            new_sub.insert((*path).to_string(), *e);
        }
    }
    let raws = diff_snapshots(&old_sub, &new_sub);
    for key in old_sub.keys() {
        snapshot.remove(key);
    }
    for (key, entry) in new_sub {
        snapshot.insert(key, entry);
    }
    raws
}

// ---------------------------------------------------------------------------
// Filesystem walks for the cache coordinator
// ---------------------------------------------------------------------------

/// Recursive (total_bytes, file_count, dir_count) under
/// `dir`. Symlinks count as files and are not followed.
pub fn walk_folder_size(dir: &std::path::Path) -> (u64, u64, u64) {
    let mut total: u64 = 0;
    let mut files: u64 = 0;
    let mut dirs: u64 = 0;
    let mut stack: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let read = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.filter_map(|e| e.ok()) {
            let path = entry.path();
            let md = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.file_type().is_dir() {
                dirs += 1;
                stack.push(path);
            } else {
                files += 1;
                total = total.saturating_add(md.len());
            }
        }
    }
    (total, files, dirs)
}

/// All paths at/under `root` (files and directories,
/// sorted) whose full path contains `substring`,
/// case-insensitively. Unreadable entries are skipped.
pub fn walk_search(root: &std::path::Path, substring: &str) -> Vec<String> {
    let needle = substring.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let read = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut paths: Vec<std::path::PathBuf> =
            read.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            let is_dir = std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false);
            let text = path.to_string_lossy().into_owned();
            if text.to_lowercase().contains(&needle) {
                out.push(text.clone());
            }
            if is_dir {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// The watcher handle
// ---------------------------------------------------------------------------

struct State {
    snapshot: Snapshot,
    pending: std::collections::VecDeque<Event>,
    next_seq: u64,
    dropped: u64,
    events_emitted: u64,
    scans: u64,
    cache: Cache,
    cache_hits: u64,
    cache_misses: u64,
    invalidations: u64,
    /// Bumped whenever a scan produces any raw event, so
    /// a folder walk that raced a change is not cached.
    cache_epoch: u64,
    shutdown: bool,
}

pub(crate) struct Inner {
    pub(crate) config: Config,
    pub(crate) state: std::sync::Mutex<State>,
    pub(crate) changed: std::sync::Condvar,
    /// Serializes whole scans (walk + diff + merge) so a
    /// background scan and a manual `scan_once` can never
    /// apply snapshots out of order.
    pub(crate) scan_lock: std::sync::Mutex<()>,
    /// The backend actually running, as a `RunningBackend`
    /// discriminant (0 = polling, 1 = native). Atomic so
    /// the native thread's failure takeover can flip it to
    /// polling — and be reported honestly — without taking
    /// locks in a delicate order.
    pub(crate) backend: std::sync::atomic::AtomicU8,
    /// The polling thread's join handle when polling is
    /// the background driver: spawned at create time, or
    /// later by the native thread if the native backend
    /// dies unexpectedly (the takeover path). The native
    /// thread's own handle lives in the `Watcher`.
    pub(crate) bg: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

const BACKEND_POLLING: u8 = 0;
const BACKEND_NATIVE: u8 = 1;

/// The backend this watcher is actually running right now.
pub(crate) fn running_backend(inner: &Inner) -> RunningBackend {
    if inner.backend.load(std::sync::atomic::Ordering::SeqCst) == BACKEND_NATIVE {
        RunningBackend::Native
    } else {
        RunningBackend::Polling
    }
}

pub(crate) fn set_running_backend(inner: &Inner, backend: RunningBackend) {
    let v = match backend {
        RunningBackend::Polling => BACKEND_POLLING,
        RunningBackend::Native => BACKEND_NATIVE,
    };
    inner.backend.store(v, std::sync::atomic::Ordering::SeqCst);
}

/// What one scan observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanSummary {
    pub files_tracked: usize,
    pub raw_events: usize,
}

/// The watcher handle (`WaWatcher` in the header). All
/// state is serialized inside; every method is safe to
/// call from any thread. The background thread — polling
/// or native — (when `poll_interval_ms > 0`) is owned by
/// the handle and joined by `shutdown` (`wa_close`).
pub struct Watcher {
    inner: std::sync::Arc<Inner>,
    /// The native backend's stop handle + thread (Windows
    /// only; always empty elsewhere). The polling thread's
    /// handle lives in `Inner::bg` so the native thread's
    /// failure takeover can install one too.
    native: std::sync::Mutex<Option<platform::NativeHandle>>,
}

impl Watcher {
    /// Validate the config, build the baseline snapshot
    /// (files that already exist emit NO events — the
    /// baseline is the starting state, not a burst of
    /// creations), and start the background thread when
    /// the interval is non-zero.
    ///
    /// Backend selection is honest: `poll_interval_ms: 0`
    /// is manual mode — no background discovery at all,
    /// and the running backend is polling (driven by
    /// `scan_once`). Otherwise `polling` runs the polling
    /// thread; `auto`/`native` try the native backend on
    /// Windows and fall back to the polling thread when it
    /// cannot start (and report polling — see
    /// `running_backend`).
    pub fn create(config: Config) -> WaResult<std::sync::Arc<Watcher>> {
        for root in &config.roots {
            let p = std::path::Path::new(root);
            match std::fs::metadata(p) {
                Ok(md) if md.is_dir() => {}
                Ok(_) => {
                    return Err((
                        ERR_INVALID_ARG,
                        format!("root \"{root}\" is not a directory"),
                    ))
                }
                Err(_) => return Err((ERR_NOT_FOUND, format!("root \"{root}\" does not exist"))),
            }
        }
        let snapshot = build_snapshot(&config);
        let inner = std::sync::Arc::new(Inner {
            config,
            state: std::sync::Mutex::new(State {
                snapshot,
                pending: std::collections::VecDeque::new(),
                next_seq: 1,
                dropped: 0,
                events_emitted: 0,
                scans: 0,
                cache: Cache::default(),
                cache_hits: 0,
                cache_misses: 0,
                invalidations: 0,
                cache_epoch: 0,
                shutdown: false,
            }),
            changed: std::sync::Condvar::new(),
            scan_lock: std::sync::Mutex::new(()),
            backend: std::sync::atomic::AtomicU8::new(BACKEND_POLLING),
            bg: std::sync::Mutex::new(None),
        });
        let watcher = std::sync::Arc::new(Watcher {
            inner: inner.clone(),
            native: std::sync::Mutex::new(None),
        });
        let interval = inner.config.poll_interval_ms;
        if interval > 0 {
            let want_native = matches!(inner.config.backend, Backend::Auto | Backend::Native);
            let mut started_native = false;
            if want_native {
                // On Windows this opens every root and
                // spawns the ReadDirectoryChangesW thread;
                // anywhere else (or when startup fails) it
                // returns None and polling takes over — and
                // the backend flag stays "polling", so the
                // fallback is reported, never hidden.
                if let Some(handle) = platform::try_start_native(inner.clone()) {
                    set_running_backend(&inner, RunningBackend::Native);
                    *lock(&watcher.native) = Some(handle);
                    started_native = true;
                }
            }
            if !started_native {
                spawn_polling(&inner);
            }
        }
        Ok(watcher)
    }

    /// Force one synchronous scan (also what the
    /// background thread runs). Deterministic in tests.
    pub fn scan_once(&self) -> ScanSummary {
        do_scan(&self.inner)
    }

    /// Consume pending events. Events with `seq <=
    /// since_seq` are acknowledged: removed without being
    /// returned (they are NOT counted in `dropped`).
    /// Returns up to `max_events` of the rest (0 = no
    /// limit); returned events are removed from the
    /// queue, so a consumer never sees one twice.
    pub fn poll_events(&self, since_seq: u64, max_events: u32) -> PollResult {
        let mut g = lock(&self.inner.state);
        while matches!(g.pending.front(), Some(e) if e.seq <= since_seq) {
            g.pending.pop_front();
        }
        let limit = if max_events == 0 {
            usize::MAX
        } else {
            max_events as usize
        };
        let mut events = Vec::new();
        while events.len() < limit {
            match g.pending.pop_front() {
                Some(e) => events.push(e),
                None => break,
            }
        }
        let next_seq = events.last().map(|e| e.seq).unwrap_or(since_seq);
        PollResult {
            next_seq,
            dropped: g.dropped,
            queue_len: g.pending.len(),
            events,
        }
    }

    /// Recursive size of a folder, cached. Any later event
    /// at/under the folder (or above it) invalidates the
    /// entry, so a hit is never stale past the next scan.
    pub fn folder_size(&self, path: &str) -> WaResult<FolderSize> {
        let norm = normalize_path_str(path);
        let p = std::path::Path::new(&norm);
        match std::fs::metadata(p) {
            Ok(md) if md.is_dir() => {}
            Ok(_) => return Err((ERR_INVALID_ARG, format!("\"{path}\" is not a directory"))),
            Err(_) => return Err((ERR_NOT_FOUND, format!("\"{path}\" does not exist"))),
        }
        let key = format!("size\u{1}{norm}");
        let epoch = {
            let mut g = lock(&self.inner.state);
            if let Some(CacheValue::FolderSize {
                total_bytes,
                file_count,
                dir_count,
            }) = g.cache.get(&key)
            {
                g.cache_hits += 1;
                return Ok(FolderSize {
                    path: norm,
                    total_bytes,
                    file_count,
                    dir_count,
                    cached: true,
                });
            }
            g.cache_misses += 1;
            g.cache_epoch
        };
        let (total_bytes, file_count, dir_count) = walk_folder_size(p);
        let mut g = lock(&self.inner.state);
        // Cache the result only if no change raced the
        // walk; a raced result is returned uncached.
        let raced = g.cache_epoch != epoch;
        if !raced {
            g.cache.insert(
                key,
                norm.clone(),
                CacheValue::FolderSize {
                    total_bytes,
                    file_count,
                    dir_count,
                },
                self.inner.config.max_cache_entries,
            );
        }
        Ok(FolderSize {
            path: norm,
            total_bytes,
            file_count,
            dir_count,
            cached: false,
        })
    }

    /// Paths under `root` containing `substring`
    /// (case-insensitive), cached and invalidated exactly
    /// like folder sizes. The pattern stands in for
    /// thumbnail/search caches in a host: compute once,
    /// serve hits, let the event stream invalidate.
    pub fn cached_search(&self, root: &str, substring: &str) -> WaResult<SearchResult> {
        let norm = normalize_path_str(root);
        let p = std::path::Path::new(&norm);
        match std::fs::metadata(p) {
            Ok(md) if md.is_dir() => {}
            Ok(_) => return Err((ERR_INVALID_ARG, format!("\"{root}\" is not a directory"))),
            Err(_) => return Err((ERR_NOT_FOUND, format!("\"{root}\" does not exist"))),
        }
        let key = format!("search\u{1}{norm}\u{1}{}", substring.to_lowercase());
        let epoch = {
            let mut g = lock(&self.inner.state);
            if let Some(CacheValue::Search { matches }) = g.cache.get(&key) {
                g.cache_hits += 1;
                return Ok(SearchResult {
                    root: norm,
                    substring: substring.to_string(),
                    matches,
                    cached: true,
                });
            }
            g.cache_misses += 1;
            g.cache_epoch
        };
        let matches = walk_search(p, substring);
        let mut g = lock(&self.inner.state);
        let raced = g.cache_epoch != epoch;
        if !raced {
            g.cache.insert(
                key,
                norm.clone(),
                CacheValue::Search {
                    matches: matches.clone(),
                },
                self.inner.config.max_cache_entries,
            );
        }
        Ok(SearchResult {
            root: norm,
            substring: substring.to_string(),
            matches,
            cached: false,
        })
    }

    pub fn status(&self) -> Status {
        let g = lock(&self.inner.state);
        Status {
            roots: self.inner.config.roots.clone(),
            poll_interval_ms: self.inner.config.poll_interval_ms,
            backend: running_backend(&self.inner),
            files_tracked: g.snapshot.len(),
            events_emitted: g.events_emitted,
            queue_len: g.pending.len(),
            dropped: g.dropped,
            next_seq: g.next_seq,
            scans: g.scans,
            cache_entries: g.cache.len(),
            cache_hits: g.cache_hits,
            cache_misses: g.cache_misses,
            invalidations: g.invalidations,
        }
    }

    /// Signal the background thread(s) and join them.
    /// Never returns while a thread it owns is alive; a
    /// second call (or a call on a manual-mode watcher) is
    /// a no-op.
    pub fn shutdown(&self) {
        {
            let mut g = lock(&self.inner.state);
            g.shutdown = true;
        }
        self.inner.changed.notify_all();
        // Native first: SetEvent wakes its wait instantly,
        // and joining it before the polling handle matters
        // because a dying native thread may install the
        // polling takeover handle it must be joined through.
        if let Some(mut h) = lock(&self.native).take() {
            h.stop_and_join();
        }
        let handle = lock(&self.inner.bg).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

/// The polling background loop: wait one interval (or
/// until notified), scan, repeat until shutdown. Runs on
/// the create-time polling thread and on the native
/// thread's failure-takeover thread alike.
fn polling_loop(inner: std::sync::Arc<Inner>) {
    let interval = inner.config.poll_interval_ms;
    loop {
        let stop = {
            let g = lock(&inner.state);
            let (g2, _) = inner
                .changed
                .wait_timeout(g, std::time::Duration::from_millis(interval))
                .unwrap_or_else(|e| e.into_inner());
            g2.shutdown
        };
        if stop {
            break;
        }
        do_scan(&inner);
    }
}

/// Spawn the polling thread (if none is running) and
/// record its handle for `shutdown` to join. Used at
/// create time and by the native backend's failure
/// takeover; the second caller finds the slot taken and
/// does nothing.
pub(crate) fn spawn_polling(inner: &std::sync::Arc<Inner>) {
    let mut slot = lock(&inner.bg);
    if slot.is_some() {
        return;
    }
    let bg = inner.clone();
    *slot = Some(std::thread::spawn(move || polling_loop(bg)));
}

/// The native thread's last act when it dies
/// unexpectedly: discovery must not silently stop, and
/// the status must not keep claiming "native". Unless
/// the watcher is already shutting down (then `shutdown`
/// owns every thread), flip the reported backend to
/// polling and install the polling thread as the driver.
/// Called by `src/platform.rs` only — which is why host
/// (non-Windows) builds see it as unused.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn native_failed_takeover(inner: &std::sync::Arc<Inner>) {
    let shutting_down = lock(&inner.state).shutdown;
    if shutting_down {
        return;
    }
    set_running_backend(inner, RunningBackend::Polling);
    spawn_polling(inner);
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The FFI close path runs shutdown explicitly;
        // this keeps a purely-Rust consumer that forgets
        // from leaking the polling thread too.
        self.shutdown();
    }
}

/// The shared tail of every change path: invalidate the
/// cache entries the raw events touch (at scan/apply
/// time, never poll time), bump the cache epoch, and
/// coalesce the events into the pending queue. Polling
/// scans, per-root rescans, and native-backend batches
/// all converge here, so the contracts live in exactly
/// one place.
fn apply_raw_events(inner: &Inner, g: &mut State, raws: &[RawEvent]) {
    if raws.is_empty() {
        return;
    }
    g.cache_epoch += 1;
    for raw in raws {
        let removed = g.cache.invalidate_for(&raw.path, raw.old_path.as_deref());
        g.invalidations += removed as u64;
    }
    // Split the state's fields so merge_one can hold
    // its borrows without fighting the borrow checker.
    let mut dropped = g.dropped;
    let State {
        pending,
        next_seq,
        events_emitted,
        ..
    } = &mut *g;
    for raw in raws {
        merge_one(
            pending,
            raw,
            next_seq,
            events_emitted,
            inner.config.max_queue,
            &mut dropped,
        );
    }
    // The field borrows above have ended (NLL), so the
    // accumulated drop count can be written back.
    g.dropped = dropped;
}

fn do_scan(inner: &Inner) -> ScanSummary {
    let _serialized = lock(&inner.scan_lock);
    let new_snapshot = build_snapshot(&inner.config);
    let mut g = lock(&inner.state);
    let raws = diff_snapshots(&g.snapshot, &new_snapshot);
    let raw_count = raws.len();
    g.snapshot = new_snapshot;
    g.scans += 1;
    apply_raw_events(inner, &mut g, &raws);
    ScanSummary {
        files_tracked: g.snapshot.len(),
        raw_events: raw_count,
    }
}

/// Rebuild one root's snapshot from disk and diff it
/// against the recorded state (the rest of the snapshot
/// is untouched). The native backend runs this when the
/// OS change buffer overflows: whatever was lost is
/// rediscovered by the ordinary polling machinery, so an
/// overflow degrades to one root walk, never to silently
/// missed changes. Counts as a scan in `wa_status`.
/// (Called from the Windows backend and from tests.)
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn do_root_rescan(inner: &Inner, root: &str) -> ScanSummary {
    let _serialized = lock(&inner.scan_lock);
    let roots = [root.to_string()];
    let fresh = build_snapshot_of(&inner.config, &roots);
    let mut g = lock(&inner.state);
    let mut old_sub: Snapshot = Snapshot::new();
    for key in g.snapshot.keys() {
        if at_or_under(key, root) {
            old_sub.insert(key.clone(), g.snapshot[key]);
        }
    }
    let raws = diff_snapshots(&old_sub, &fresh);
    let raw_count = raws.len();
    for key in old_sub.keys() {
        g.snapshot.remove(key);
    }
    for (key, entry) in fresh {
        g.snapshot.insert(key, entry);
    }
    g.scans += 1;
    apply_raw_events(inner, &mut g, &raws);
    ScanSummary {
        files_tracked: g.snapshot.len(),
        raw_events: raw_count,
    }
}

/// Apply one batch of OS-named changes (already statted
/// by the caller into `merge_observed_into_snapshot`'s
/// input shape) through the shared pipeline. Serialized
/// with scans on the scan lock so a native batch can
/// never interleave with a walk+diff halfway. A batch is
/// not a scan — `scans` does not move.
/// (Called from the Windows backend and from tests.)
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn apply_observed_changes(inner: &Inner, observed: &[(String, Option<SnapshotEntry>)]) {
    if observed.is_empty() {
        return;
    }
    let _serialized = lock(&inner.scan_lock);
    let mut g = lock(&inner.state);
    let raws = merge_observed_into_snapshot(&mut g.snapshot, observed);
    apply_raw_events(inner, &mut g, &raws);
}

// ---------------------------------------------------------------------------
// Result shapes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PollResult {
    pub events: Vec<Event>,
    /// Pass this back as `since_seq` on the next poll.
    pub next_seq: u64,
    /// Total events dropped to queue overflow so far.
    pub dropped: u64,
    pub queue_len: usize,
}

impl PollResult {
    pub fn to_json(&self) -> String {
        let evs: Vec<String> = self.events.iter().map(|e| e.to_json()).collect();
        format!(
            "{{\"events\":[{}],\"next_seq\":{},\"dropped\":{},\"queue_len\":{}}}",
            evs.join(","),
            self.next_seq,
            self.dropped,
            self.queue_len
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FolderSize {
    pub path: String,
    pub total_bytes: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub cached: bool,
}

impl FolderSize {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"path\":\"{}\",\"total_bytes\":{},\"file_count\":{},\"dir_count\":{},\"cached\":{}}}",
            json_escape(&self.path),
            self.total_bytes,
            self.file_count,
            self.dir_count,
            self.cached
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchResult {
    pub root: String,
    pub substring: String,
    pub matches: Vec<String>,
    pub cached: bool,
}

impl SearchResult {
    pub fn to_json(&self) -> String {
        let ms: Vec<String> = self
            .matches
            .iter()
            .map(|m| format!("\"{}\"", json_escape(m)))
            .collect();
        format!(
            "{{\"root\":\"{}\",\"substring\":\"{}\",\"matches\":[{}],\"count\":{},\"cached\":{}}}",
            json_escape(&self.root),
            json_escape(&self.substring),
            ms.join(","),
            self.matches.len(),
            self.cached
        )
    }
}

#[derive(Clone, Debug)]
pub struct Status {
    pub roots: Vec<String>,
    pub poll_interval_ms: u64,
    /// The backend actually running (never just the one
    /// the config requested).
    pub backend: RunningBackend,
    pub files_tracked: usize,
    /// Distinct events ever queued (sequence numbers
    /// handed out). Coalesced merges do not add to it; a
    /// net-zero removal does not subtract.
    pub events_emitted: u64,
    pub queue_len: usize,
    pub dropped: u64,
    /// The sequence number the next new event will get.
    pub next_seq: u64,
    pub scans: u64,
    pub cache_entries: usize,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub invalidations: u64,
}

impl Status {
    pub fn to_json(&self) -> String {
        let roots: Vec<String> = self
            .roots
            .iter()
            .map(|r| format!("\"{}\"", json_escape(r)))
            .collect();
        format!(
            "{{\"roots\":[{}],\"poll_interval_ms\":{},\"backend\":\"{}\",\"files_tracked\":{},\"events_emitted\":{},\"queue_len\":{},\"dropped\":{},\"next_seq\":{},\"scans\":{},\"cache_entries\":{},\"cache_hits\":{},\"cache_misses\":{},\"invalidations\":{}}}",
            roots.join(","),
            self.poll_interval_ms,
            self.backend.as_str(),
            self.files_tracked,
            self.events_emitted,
            self.queue_len,
            self.dropped,
            self.next_seq,
            self.scans,
            self.cache_entries,
            self.cache_hits,
            self.cache_misses,
            self.invalidations
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory per test, removed on drop.
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> TestDir {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir()
                .join(format!("watchkit-test-{}-{name}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }
        fn s(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
        fn join(&self, rel: &str) -> std::path::PathBuf {
            self.0.join(rel)
        }
        fn file(&self, rel: &str) -> String {
            self.join(rel).to_string_lossy().into_owned()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn manual_config(dir: &TestDir) -> Config {
        parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0}}",
            json_escape(&dir.s())
        ))
        .unwrap()
    }

    fn write(path: &std::path::Path, data: &[u8]) {
        std::fs::write(path, data).unwrap();
    }

    fn append(path: &std::path::Path, data: &[u8]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(data).unwrap();
    }

    // -- Basics -------------------------------------------------------

    #[test]
    fn escapes_json() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn config_parse_rules() {
        let d = TestDir::new("cfg");
        let ok = parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":250,\"ignore\":[\"*.tmp\"],\"max_queue\":7,\"max_cache_entries\":0}}",
            json_escape(&d.s())
        ))
        .unwrap();
        assert_eq!(ok.poll_interval_ms, 250);
        assert_eq!(ok.max_queue, 7);
        assert_eq!(ok.max_cache_entries, 0);

        let defaults =
            parse_config(&format!("{{\"roots\":[\"{}\"]}}", json_escape(&d.s()))).unwrap();
        assert_eq!(defaults.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
        assert_eq!(defaults.max_queue, DEFAULT_MAX_QUEUE);
        assert_eq!(defaults.max_cache_entries, DEFAULT_MAX_CACHE_ENTRIES);

        assert_eq!(parse_config("{\"roots\":").unwrap_err().0, ERR_PARSE); // malformed
        assert_eq!(
            parse_config(&format!(
                "{{\"roots\":[\"{}\"],\"bogus\":1}}",
                json_escape(&d.s())
            ))
            .unwrap_err()
            .0,
            ERR_PARSE // unknown key
        );
        assert_eq!(
            parse_config(&format!(
                "{{\"roots\":[\"{}\"],\"roots\":[\"{}\"]}}",
                json_escape(&d.s()),
                json_escape(&d.s())
            ))
            .unwrap_err()
            .0,
            ERR_PARSE // duplicate key (family rule)
        );
        assert_eq!(parse_config("{}").unwrap_err().0, ERR_INVALID_ARG); // missing roots
        assert_eq!(
            parse_config("{\"roots\":[]}").unwrap_err().0,
            ERR_INVALID_ARG // empty roots
        );
        assert_eq!(
            parse_config("{\"roots\":\"x\"}").unwrap_err().0,
            ERR_INVALID_ARG // wrong type
        );
        assert_eq!(
            parse_config(&format!(
                "{{\"roots\":[\"{}\"],\"max_queue\":0}}",
                json_escape(&d.s())
            ))
            .unwrap_err()
            .0,
            ERR_INVALID_ARG // queue must hold >= 1
        );
        assert_eq!(
            parse_config(&format!(
                "{{\"roots\":[\"{}\"],\"poll_interval_ms\":-1}}",
                json_escape(&d.s())
            ))
            .unwrap_err()
            .0,
            ERR_INVALID_ARG
        );
    }

    #[test]
    fn config_backend_key() {
        let d = TestDir::new("backend");
        let root = json_escape(&d.s());
        // Absent = auto; each documented value parses.
        let def = parse_config(&format!("{{\"roots\":[\"{root}\"]}}")).unwrap();
        assert_eq!(def.backend, Backend::Auto);
        for (text, want) in [
            ("auto", Backend::Auto),
            ("native", Backend::Native),
            ("polling", Backend::Polling),
        ] {
            let c = parse_config(&format!(
                "{{\"roots\":[\"{root}\"],\"backend\":\"{text}\"}}"
            ))
            .unwrap();
            assert_eq!(c.backend, want);
        }
        // Invalid values follow the family convention for
        // wrong-typed / out-of-range values: INVALID_ARG.
        assert_eq!(
            parse_config(&format!("{{\"roots\":[\"{root}\"],\"backend\":\"fast\"}}"))
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_config(&format!("{{\"roots\":[\"{root}\"],\"backend\":3}}"))
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
    }

    #[test]
    fn backend_manual_mode_reports_polling() {
        // Even with "native" requested, poll_interval_ms 0
        // is manual mode: no background discovery exists,
        // scan_once (the poller) is the driver, and the
        // status says polling — on every platform.
        let d = TestDir::new("backend-manual");
        let cfg = parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0,\"backend\":\"native\"}}",
            json_escape(&d.s())
        ))
        .unwrap();
        let w = Watcher::create(cfg).unwrap();
        assert_eq!(w.status().backend, RunningBackend::Polling);
        assert!(w.status().to_json().contains("\"backend\":\"polling\""));
    }

    fn snap_entry(size: u64, mtime: u128, is_dir: bool) -> SnapshotEntry {
        SnapshotEntry {
            size,
            mtime_nanos: mtime,
            is_dir,
        }
    }

    #[test]
    fn merge_observed_upsert_modify_remove() {
        let mut snap: Snapshot = Snapshot::new();
        snap.insert("/r/a.txt".to_string(), snap_entry(5, 100, false));
        let raws = merge_observed_into_snapshot(
            &mut snap,
            &[
                ("/r/a.txt".to_string(), Some(snap_entry(7, 200, false))),
                ("/r/b.txt".to_string(), Some(snap_entry(3, 300, false))),
                // A removal for a path never tracked is a
                // no-op (the native net-zero case).
                ("/r/never.txt".to_string(), None),
            ],
        );
        assert_eq!(raws.len(), 2);
        assert!(raws
            .iter()
            .any(|r| r.kind == EventKind::Modified && r.path == "/r/a.txt"));
        assert!(raws
            .iter()
            .any(|r| r.kind == EventKind::Created && r.path == "/r/b.txt"));
        assert_eq!(snap["/r/a.txt"], snap_entry(7, 200, false));
        assert_eq!(snap["/r/b.txt"], snap_entry(3, 300, false));
    }

    #[test]
    fn merge_observed_pairs_rename_and_removes_dir_subtree() {
        let mut snap: Snapshot = Snapshot::new();
        snap.insert("/r/old.txt".to_string(), snap_entry(5, 100, false));
        snap.insert("/r/d".to_string(), snap_entry(0, 50, true));
        snap.insert("/r/d/x.txt".to_string(), snap_entry(1, 60, false));
        snap.insert("/r/keep.txt".to_string(), snap_entry(2, 70, false));
        // RENAMED_OLD + RENAMED_NEW in one batch pair into
        // a single renamed event, exactly as a polling
        // diff of the same change pairs them.
        let raws = merge_observed_into_snapshot(
            &mut snap,
            &[
                ("/r/old.txt".to_string(), None),
                ("/r/new.txt".to_string(), Some(snap_entry(5, 100, false))),
            ],
        );
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].kind, EventKind::Renamed);
        assert_eq!(raws[0].path, "/r/new.txt");
        assert_eq!(raws[0].old_path.as_deref(), Some("/r/old.txt"));
        // Removing a directory removes its whole subtree
        // from the snapshot, one deleted event per entry.
        let raws = merge_observed_into_snapshot(&mut snap, &[("/r/d".to_string(), None)]);
        assert_eq!(raws.len(), 2);
        assert!(raws.iter().all(|r| r.kind == EventKind::Deleted));
        assert!(!snap.contains_key("/r/d"));
        assert!(!snap.contains_key("/r/d/x.txt"));
        assert!(snap.contains_key("/r/keep.txt"));
        assert!(snap.contains_key("/r/new.txt"));
    }

    #[test]
    fn merge_observed_folds_duplicates_last_wins() {
        let mut snap: Snapshot = Snapshot::new();
        // Added and removed inside one batch, path never
        // tracked: folds to a removal of nothing = silence.
        let raws = merge_observed_into_snapshot(
            &mut snap,
            &[
                ("/r/t.txt".to_string(), Some(snap_entry(1, 1, false))),
                ("/r/t.txt".to_string(), None),
            ],
        );
        assert!(raws.is_empty());
        assert!(snap.is_empty());
        // Removed and re-added with identical state in one
        // batch: no diff, no event.
        snap.insert("/r/t.txt".to_string(), snap_entry(1, 1, false));
        let raws = merge_observed_into_snapshot(
            &mut snap,
            &[
                ("/r/t.txt".to_string(), None),
                ("/r/t.txt".to_string(), Some(snap_entry(1, 1, false))),
            ],
        );
        assert!(raws.is_empty());
        assert_eq!(snap["/r/t.txt"], snap_entry(1, 1, false));
    }

    #[test]
    fn root_rescan_applies_one_roots_diff() {
        let d = TestDir::new("rescan");
        write(&d.join("a.txt"), b"aaa");
        let w = Watcher::create(manual_config(&d)).unwrap();
        write(&d.join("b.txt"), b"b");
        std::fs::remove_file(d.join("a.txt")).unwrap();
        // A rescan of the root (the native overflow path)
        // produces exactly the events a full scan would,
        // and counts as a scan.
        let summary = do_root_rescan(&w.inner, &d.s());
        assert_eq!(summary.raw_events, 2);
        assert_eq!(w.status().scans, 1);
        let poll = w.poll_events(0, 0);
        let mut kinds: Vec<&str> = poll.events.iter().map(|e| e.kind.as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["created", "deleted"]);
    }

    #[test]
    fn apply_observed_changes_feeds_the_shared_pipeline() {
        let d = TestDir::new("observed");
        let w = Watcher::create(manual_config(&d)).unwrap();
        // Cache the (empty) root's folder size first, so
        // the batch's invalidation is observable.
        let _ = w.folder_size(&d.s()).unwrap();
        write(&d.join("new.txt"), b"data");
        let md = std::fs::symlink_metadata(d.join("new.txt")).unwrap();
        let entry = SnapshotEntry {
            size: md.len(),
            mtime_nanos: mtime_nanos_of(&md),
            is_dir: false,
        };
        apply_observed_changes(&w.inner, &[(d.file("new.txt"), Some(entry))]);
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 1);
        assert_eq!(poll.events[0].kind, EventKind::Created);
        let st = w.status();
        assert_eq!(st.files_tracked, 1);
        // Invalidation happened through the shared
        // scan-time rule, and a batch is not a scan.
        assert_eq!(st.invalidations, 1);
        assert_eq!(st.scans, 0);
    }

    #[test]
    fn glob_rules() {
        assert!(glob_match("*.tmp", "notes.tmp"));
        assert!(!glob_match("*.tmp", "notes.txt"));
        assert!(!glob_match("*.tmp", "sub/notes.tmp")); // * does not cross /
        assert!(glob_match("**/*.tmp", "sub/notes.tmp"));
        assert!(glob_match(".git/**", ".git/config"));
        assert!(glob_match(".git/**", ".git/refs/heads/main"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));

        let pats = vec!["*.tmp".to_string(), ".git/**".to_string()];
        assert!(is_ignored("notes.tmp", &pats));
        assert!(is_ignored("deep/notes.tmp", &pats));
        assert!(is_ignored(".git", &pats));
        assert!(is_ignored(".git/config", &pats));
        assert!(is_ignored("repo/.git/objects/ab", &pats));
        assert!(!is_ignored("keep.txt", &pats));
        assert!(!is_ignored("git/keep.txt", &pats));
    }

    // -- Detection ----------------------------------------------------

    #[test]
    fn baseline_emits_no_events() {
        let d = TestDir::new("baseline");
        write(&d.join("pre.txt"), b"already here");
        let w = Watcher::create(manual_config(&d)).unwrap();
        let st = w.status();
        assert_eq!(st.files_tracked, 1);
        assert_eq!(st.queue_len, 0);
        w.scan_once();
        assert_eq!(w.poll_events(0, 0).events.len(), 0);
        w.shutdown();
    }

    #[test]
    fn create_modify_delete_detection() {
        let d = TestDir::new("cmd");
        let w = Watcher::create(manual_config(&d)).unwrap();

        // Create.
        write(&d.join("a.txt"), b"hello");
        let s = w.scan_once();
        assert_eq!(s.raw_events, 1);
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 1);
        assert_eq!(poll.events[0].kind, EventKind::Created);
        assert_eq!(poll.events[0].path, d.file("a.txt"));
        assert_eq!(poll.events[0].seq, 1);
        assert_eq!(poll.events[0].coalesced_count, 1);
        assert_eq!(poll.next_seq, 1);
        assert_eq!(poll.dropped, 0);
        // Consumed: a second poll returns nothing.
        assert_eq!(w.poll_events(1, 0).events.len(), 0);

        // Modify (size grows, so detection does not
        // depend on mtime granularity).
        append(&d.join("a.txt"), b" world");
        w.scan_once();
        let poll = w.poll_events(1, 0);
        assert_eq!(poll.events.len(), 1);
        assert_eq!(poll.events[0].kind, EventKind::Modified);
        assert_eq!(poll.events[0].seq, 2);

        // Subdirectory + nested file: dir created, file
        // created, no dir-modified noise.
        std::fs::create_dir(d.join("sub")).unwrap();
        write(&d.join("sub/b.txt"), b"x");
        w.scan_once();
        let poll = w.poll_events(2, 0);
        assert_eq!(poll.events.len(), 2);
        assert!(poll.events.iter().all(|e| e.kind == EventKind::Created));

        // Delete.
        std::fs::remove_file(d.join("a.txt")).unwrap();
        w.scan_once();
        let poll = w.poll_events(poll.next_seq, 0);
        assert_eq!(poll.events.len(), 1);
        assert_eq!(poll.events[0].kind, EventKind::Deleted);
        assert_eq!(poll.events[0].path, d.file("a.txt"));
        w.shutdown();
    }

    #[test]
    fn rename_pairing() {
        let d = TestDir::new("rename");
        write(&d.join("old.txt"), b"payload-data");
        let w = Watcher::create(manual_config(&d)).unwrap();
        std::fs::rename(d.join("old.txt"), d.join("new.txt")).unwrap();
        w.scan_once();
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 1, "events: {:?}", poll.events);
        assert_eq!(poll.events[0].kind, EventKind::Renamed);
        assert_eq!(poll.events[0].path, d.file("new.txt"));
        assert_eq!(
            poll.events[0].old_path.as_deref(),
            Some(d.file("old.txt").as_str())
        );
        w.shutdown();
    }

    #[test]
    fn unrelated_delete_create_same_scan_do_not_pair_when_sizes_differ() {
        let d = TestDir::new("nopair");
        write(&d.join("gone.txt"), b"12345");
        let w = Watcher::create(manual_config(&d)).unwrap();
        std::fs::remove_file(d.join("gone.txt")).unwrap();
        write(&d.join("fresh.txt"), b"different-size-content");
        w.scan_once();
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 2);
        assert!(poll
            .events
            .iter()
            .any(|e| e.kind == EventKind::Deleted && e.path == d.file("gone.txt")));
        assert!(poll
            .events
            .iter()
            .any(|e| e.kind == EventKind::Created && e.path == d.file("fresh.txt")));
        w.shutdown();
    }

    // -- Coalescing -----------------------------------------------------

    #[test]
    fn burst_create_plus_modifies_is_one_event_count_4() {
        let d = TestDir::new("burst");
        let w = Watcher::create(manual_config(&d)).unwrap();
        write(&d.join("f.bin"), b"1");
        w.scan_once(); // created
        append(&d.join("f.bin"), b"2");
        w.scan_once(); // modified
        append(&d.join("f.bin"), b"3");
        w.scan_once(); // modified
        append(&d.join("f.bin"), b"4");
        w.scan_once(); // modified
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 1, "events: {:?}", poll.events);
        assert_eq!(poll.events[0].kind, EventKind::Created);
        assert_eq!(poll.events[0].coalesced_count, 4);
        assert_eq!(poll.events[0].seq, 1);
        w.shutdown();
    }

    #[test]
    fn net_zero_create_then_delete_is_no_event() {
        let d = TestDir::new("netzero");
        let w = Watcher::create(manual_config(&d)).unwrap();
        write(&d.join("ephemeral.txt"), b"gone soon");
        w.scan_once();
        std::fs::remove_file(d.join("ephemeral.txt")).unwrap();
        w.scan_once();
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 0, "events: {:?}", poll.events);
        assert_eq!(w.status().queue_len, 0);
        w.shutdown();
    }

    #[test]
    fn modified_then_deleted_is_one_deleted() {
        let d = TestDir::new("moddel");
        write(&d.join("v.txt"), b"v1");
        let w = Watcher::create(manual_config(&d)).unwrap();
        append(&d.join("v.txt"), b"-v2");
        w.scan_once(); // modified (pending)
        std::fs::remove_file(d.join("v.txt")).unwrap();
        w.scan_once(); // deleted merges over the modified
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 1);
        assert_eq!(poll.events[0].kind, EventKind::Deleted);
        assert_eq!(poll.events[0].coalesced_count, 2);
        w.shutdown();
    }

    #[test]
    fn deleted_then_recreated_is_one_modified() {
        let d = TestDir::new("delrecreate");
        write(&d.join("r.txt"), b"original");
        let w = Watcher::create(manual_config(&d)).unwrap();
        std::fs::remove_file(d.join("r.txt")).unwrap();
        w.scan_once(); // deleted (pending)
        write(&d.join("r.txt"), b"replacement!");
        w.scan_once(); // created merges over the deleted
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 1, "events: {:?}", poll.events);
        assert_eq!(poll.events[0].kind, EventKind::Modified);
        assert_eq!(poll.events[0].coalesced_count, 2);
        w.shutdown();
    }

    #[test]
    fn create_then_rename_is_one_created_at_new_name() {
        let d = TestDir::new("createrename");
        let w = Watcher::create(manual_config(&d)).unwrap();
        write(&d.join("draft.txt"), b"content-here");
        w.scan_once(); // created draft.txt (pending)
        std::fs::rename(d.join("draft.txt"), d.join("final.txt")).unwrap();
        w.scan_once(); // renamed merges over the created
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 1, "events: {:?}", poll.events);
        assert_eq!(poll.events[0].kind, EventKind::Created);
        assert_eq!(poll.events[0].path, d.file("final.txt"));
        assert_eq!(poll.events[0].coalesced_count, 2);
        w.shutdown();
    }

    #[test]
    fn rename_then_delete_is_deleted_of_original() {
        let d = TestDir::new("renamedel");
        write(&d.join("orig.txt"), b"some-bytes!");
        let w = Watcher::create(manual_config(&d)).unwrap();
        std::fs::rename(d.join("orig.txt"), d.join("moved.txt")).unwrap();
        w.scan_once(); // renamed (pending)
        std::fs::remove_file(d.join("moved.txt")).unwrap();
        w.scan_once(); // deleted merges over the renamed
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 1, "events: {:?}", poll.events);
        assert_eq!(poll.events[0].kind, EventKind::Deleted);
        assert_eq!(poll.events[0].path, d.file("orig.txt"));
        assert_eq!(poll.events[0].coalesced_count, 2);
        w.shutdown();
    }

    #[test]
    fn poll_since_seq_and_max_events() {
        let d = TestDir::new("poll");
        let w = Watcher::create(manual_config(&d)).unwrap();
        for name in ["1.txt", "2.txt", "3.txt"] {
            write(&d.join(name), b"x");
        }
        w.scan_once();
        let first = w.poll_events(0, 2);
        assert_eq!(first.events.len(), 2);
        assert_eq!(first.next_seq, 2);
        assert_eq!(first.queue_len, 1);
        let rest = w.poll_events(first.next_seq, 0);
        assert_eq!(rest.events.len(), 1);
        assert_eq!(rest.events[0].seq, 3);
        assert_eq!(rest.next_seq, 3);
        // since_seq acknowledges: a stale pending event at
        // or below it is dropped from the queue unseen.
        write(&d.join("4.txt"), b"x");
        w.scan_once();
        let skipped = w.poll_events(4, 0);
        assert_eq!(skipped.events.len(), 0);
        assert_eq!(w.status().queue_len, 0);
        assert_eq!(skipped.dropped, 0); // acknowledges are not overflow drops
        w.shutdown();
    }

    #[test]
    fn queue_overflow_drops_oldest_and_counts() {
        let d = TestDir::new("overflow");
        let cfg = parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0,\"max_queue\":4}}",
            json_escape(&d.s())
        ))
        .unwrap();
        let w = Watcher::create(cfg).unwrap();
        for i in 0..10 {
            write(&d.join(&format!("f{i:02}.txt")), b"x");
            w.scan_once();
        }
        let st = w.status();
        assert_eq!(st.queue_len, 4);
        assert_eq!(st.dropped, 6);
        assert_eq!(st.events_emitted, 10);
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 4);
        assert_eq!(poll.dropped, 6);
        // The survivors are the NEWEST four (seqs 7..=10).
        assert_eq!(poll.events.first().unwrap().seq, 7);
        assert_eq!(poll.events.last().unwrap().seq, 10);
        w.shutdown();
    }

    // -- Ignore globs ---------------------------------------------------

    #[test]
    fn ignore_globs_hide_files_and_prune_dirs() {
        let d = TestDir::new("ignore");
        std::fs::create_dir_all(d.join(".git/objects")).unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        let cfg = parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0,\"ignore\":[\"*.tmp\",\".git/**\"]}}",
            json_escape(&d.s())
        ))
        .unwrap();
        let w = Watcher::create(cfg).unwrap();
        write(&d.join("keep.txt"), b"k");
        write(&d.join("skip.tmp"), b"s");
        write(&d.join(".git/config"), b"g");
        write(&d.join(".git/objects/ab"), b"g");
        write(&d.join("src/nested.tmp"), b"s");
        write(&d.join("src/code.rs"), b"c");
        w.scan_once();
        let st = w.status();
        // Tracked: src dir, keep.txt, src/code.rs only.
        assert_eq!(st.files_tracked, 3, "status: {}", st.to_json());
        let poll = w.poll_events(0, 0);
        // Two creations: the src folder itself predates
        // the baseline snapshot, so it is not an event.
        assert_eq!(poll.events.len(), 2);
        assert!(poll
            .events
            .iter()
            .all(|e| !e.path.contains(".tmp") && !e.path.contains(".git")));
        // Changes inside ignored places stay invisible.
        append(&d.join("skip.tmp"), b"more");
        write(&d.join(".git/newfile"), b"g");
        let s = w.scan_once();
        assert_eq!(s.raw_events, 0);
        w.shutdown();
    }

    // -- Cache coordinator ----------------------------------------------

    #[test]
    fn folder_size_cache_hit_then_invalidation() {
        let d = TestDir::new("size");
        std::fs::create_dir(d.join("sub")).unwrap();
        write(&d.join("a.bin"), b"12345"); // 5
        write(&d.join("sub/b.bin"), b"1234567"); // 7
        let w = Watcher::create(manual_config(&d)).unwrap();

        let first = w.folder_size(&d.s()).unwrap();
        assert!(!first.cached);
        assert_eq!(first.total_bytes, 12);
        assert_eq!(first.file_count, 2);
        assert_eq!(first.dir_count, 1);

        let second = w.folder_size(&d.s()).unwrap();
        assert!(second.cached);
        assert_eq!(second.total_bytes, 12);
        let st = w.status();
        assert_eq!(st.cache_hits, 1);
        assert_eq!(st.cache_misses, 1);
        assert_eq!(st.invalidations, 0);

        // A change UNDER the cached folder invalidates it.
        write(&d.join("c.bin"), b"xyz"); // +3
        w.scan_once();
        let st = w.status();
        assert!(st.invalidations >= 1, "status: {}", st.to_json());
        assert_eq!(st.cache_entries, 0);

        let third = w.folder_size(&d.s()).unwrap();
        assert!(!third.cached);
        assert_eq!(third.total_bytes, 15);
        assert_eq!(third.file_count, 3);
        w.shutdown();
    }

    #[test]
    fn folder_size_unrelated_change_keeps_cache() {
        let d = TestDir::new("sizekeep");
        std::fs::create_dir(d.join("watched")).unwrap();
        std::fs::create_dir(d.join("other")).unwrap();
        write(&d.join("watched/a.txt"), b"aa");
        let w = Watcher::create(manual_config(&d)).unwrap();
        let target = d.file("watched");
        let first = w.folder_size(&target).unwrap();
        assert!(!first.cached);
        // Change outside the cached folder: no invalidation.
        write(&d.join("other/b.txt"), b"bb");
        w.scan_once();
        let second = w.folder_size(&target).unwrap();
        assert!(second.cached, "unrelated change must not evict");
        assert_eq!(w.status().invalidations, 0);
        w.shutdown();
    }

    #[test]
    fn search_cache_and_invalidation() {
        let d = TestDir::new("search");
        write(&d.join("report-2024.txt"), b"r");
        write(&d.join("notes.txt"), b"n");
        let w = Watcher::create(manual_config(&d)).unwrap();

        let first = w.cached_search(&d.s(), "REPORT").unwrap(); // case-insensitive
        assert!(!first.cached);
        assert_eq!(first.matches, vec![d.file("report-2024.txt")]);

        let second = w.cached_search(&d.s(), "report").unwrap();
        assert!(second.cached);
        assert_eq!(second.matches.len(), 1);

        write(&d.join("report-2025.txt"), b"r");
        w.scan_once();
        let third = w.cached_search(&d.s(), "report").unwrap();
        assert!(!third.cached, "new match must invalidate the search cache");
        assert_eq!(third.matches.len(), 2);
        w.shutdown();
    }

    #[test]
    fn cache_lru_eviction_at_bound() {
        let d = TestDir::new("lru");
        for name in ["a", "b", "c"] {
            std::fs::create_dir(d.join(name)).unwrap();
            write(&d.join(name).join("f.txt"), b"x");
        }
        let cfg = parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0,\"max_cache_entries\":2}}",
            json_escape(&d.s())
        ))
        .unwrap();
        let w = Watcher::create(cfg).unwrap();
        let pa = d.file("a");
        let pb = d.file("b");
        let pc = d.file("c");
        assert!(!w.folder_size(&pa).unwrap().cached);
        assert!(!w.folder_size(&pb).unwrap().cached);
        assert!(w.folder_size(&pa).unwrap().cached); // touch a; b is now LRU
        assert!(!w.folder_size(&pc).unwrap().cached); // evicts b
        assert_eq!(w.status().cache_entries, 2);
        assert!(w.folder_size(&pa).unwrap().cached);
        assert!(!w.folder_size(&pb).unwrap().cached, "b was the LRU entry");
        w.shutdown();
    }

    #[test]
    fn cache_zero_entries_disables_caching() {
        let d = TestDir::new("nocache");
        write(&d.join("a.txt"), b"aa");
        let cfg = parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0,\"max_cache_entries\":0}}",
            json_escape(&d.s())
        ))
        .unwrap();
        let w = Watcher::create(cfg).unwrap();
        assert!(!w.folder_size(&d.s()).unwrap().cached);
        assert!(!w.folder_size(&d.s()).unwrap().cached);
        assert_eq!(w.status().cache_entries, 0);
        assert_eq!(w.status().cache_misses, 2);
        w.shutdown();
    }

    #[test]
    fn folder_size_and_search_errors() {
        let d = TestDir::new("errors");
        write(&d.join("file.txt"), b"x");
        let w = Watcher::create(manual_config(&d)).unwrap();
        let missing = d.file("missing");
        assert_eq!(w.folder_size(&missing).unwrap_err().0, ERR_NOT_FOUND);
        assert_eq!(
            w.folder_size(&d.file("file.txt")).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(w.cached_search(&missing, "x").unwrap_err().0, ERR_NOT_FOUND);
        // Nonexistent root at create: ERR_NOT_FOUND.
        let cfg = parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0}}",
            json_escape(&missing)
        ))
        .unwrap();
        assert_eq!(
            Watcher::create(cfg).map(|_| ()).unwrap_err().0,
            ERR_NOT_FOUND
        );
        w.shutdown();
    }

    // -- Status, threading, lifecycle -----------------------------------

    #[test]
    fn status_reports_counters() {
        let d = TestDir::new("status");
        let w = Watcher::create(manual_config(&d)).unwrap();
        write(&d.join("a.txt"), b"1");
        write(&d.join("b.txt"), b"2");
        w.scan_once();
        let _ = w.folder_size(&d.s()).unwrap();
        let st = w.status();
        assert_eq!(st.roots, vec![d.s()]);
        assert_eq!(st.files_tracked, 2);
        assert_eq!(st.events_emitted, 2);
        assert_eq!(st.queue_len, 2);
        assert_eq!(st.dropped, 0);
        assert_eq!(st.scans, 1);
        assert_eq!(st.cache_misses, 1);
        assert_eq!(st.cache_entries, 1);
        let v = json::parse(&st.to_json()).unwrap();
        assert_eq!(v.get("files_tracked").unwrap().as_i64(), Some(2));
        assert_eq!(v.get("cache_hits").unwrap().as_i64(), Some(0));
        w.shutdown();
    }

    #[test]
    fn background_thread_picks_up_changes_and_close_joins() {
        let d = TestDir::new("bg");
        let cfg = parse_config(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":20}}",
            json_escape(&d.s())
        ))
        .unwrap();
        let w = Watcher::create(cfg).unwrap();
        write(&d.join("later.txt"), b"hi");
        // Wait (bounded) for the polling thread to notice;
        // the assertion is on the outcome, not the timing.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut found = false;
        while Instant::now() < deadline {
            if w.status().queue_len >= 1 {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(found, "background thread never reported the new file");
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events[0].kind, EventKind::Created);
        // Close must return (the thread is joined, never
        // detached) and be safe to call twice.
        w.shutdown();
        w.shutdown();
    }

    #[test]
    fn multiple_roots() {
        let d1 = TestDir::new("root1");
        let d2 = TestDir::new("root2");
        let cfg = parse_config(&format!(
            "{{\"roots\":[\"{}\",\"{}\"],\"poll_interval_ms\":0}}",
            json_escape(&d1.s()),
            json_escape(&d2.s())
        ))
        .unwrap();
        let w = Watcher::create(cfg).unwrap();
        write(&d1.join("one.txt"), b"1");
        write(&d2.join("two.txt"), b"2");
        w.scan_once();
        assert_eq!(w.status().files_tracked, 2);
        let poll = w.poll_events(0, 0);
        assert_eq!(poll.events.len(), 2);
        w.shutdown();
    }
}
