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
//! The engine is pure `std::fs` polling — portable and
//! deterministic (`wa_scan_once` drives it synchronously
//! in tests). A native `ReadDirectoryChangesW` backend is
//! roadmap; the coalescing and cache contracts above are
//! the product and will not change under it.

pub mod ffi;
pub mod json;

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
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

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
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

/// A fully-validated watcher configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Recursive roots (directories). Required, non-empty.
    pub roots: Vec<String>,
    /// Background poll interval. 0 disables the background
    /// thread (manual mode: scans happen only via
    /// `scan_once` — the deterministic test configuration).
    pub poll_interval_ms: u64,
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
            "roots" | "poll_interval_ms" | "ignore" | "max_queue" | "max_cache_entries"
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
    let max_queue = parse_usize_opt(&value, "max_queue", DEFAULT_MAX_QUEUE, 1)?;
    let max_cache_entries =
        parse_usize_opt(&value, "max_cache_entries", DEFAULT_MAX_CACHE_ENTRIES, 0)?;
    Ok(Config {
        roots,
        poll_interval_ms,
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

fn mtime_nanos_of(md: &std::fs::Metadata) -> u128 {
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
    let mut map = Snapshot::new();
    for root in &config.roots {
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

struct Inner {
    config: Config,
    state: std::sync::Mutex<State>,
    changed: std::sync::Condvar,
    /// Serializes whole scans (walk + diff + merge) so a
    /// background scan and a manual `scan_once` can never
    /// apply snapshots out of order.
    scan_lock: std::sync::Mutex<()>,
}

/// What one scan observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanSummary {
    pub files_tracked: usize,
    pub raw_events: usize,
}

/// The watcher handle (`WaWatcher` in the header). All
/// state is serialized inside; every method is safe to
/// call from any thread. The background polling thread
/// (when `poll_interval_ms > 0`) is owned by the handle
/// and joined by `shutdown` (`wa_close`).
pub struct Watcher {
    inner: std::sync::Arc<Inner>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Watcher {
    /// Validate the config, build the baseline snapshot
    /// (files that already exist emit NO events — the
    /// baseline is the starting state, not a burst of
    /// creations), and start the background thread when
    /// the interval is non-zero.
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
        });
        let watcher = std::sync::Arc::new(Watcher {
            inner: inner.clone(),
            thread: std::sync::Mutex::new(None),
        });
        let interval = inner.config.poll_interval_ms;
        if interval > 0 {
            let bg = inner.clone();
            let handle = std::thread::spawn(move || loop {
                let stop = {
                    let g = lock(&bg.state);
                    let (g2, _) = bg
                        .changed
                        .wait_timeout(g, std::time::Duration::from_millis(interval))
                        .unwrap_or_else(|e| e.into_inner());
                    g2.shutdown
                };
                if stop {
                    break;
                }
                do_scan(&bg);
            });
            *lock(&watcher.thread) = Some(handle);
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

    /// Signal the background thread and join it. Never
    /// returns while the thread is alive; a second call
    /// (or a call on a manual-mode watcher) is a no-op.
    pub fn shutdown(&self) {
        {
            let mut g = lock(&self.inner.state);
            g.shutdown = true;
        }
        self.inner.changed.notify_all();
        let handle = lock(&self.thread).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The FFI close path runs shutdown explicitly;
        // this keeps a purely-Rust consumer that forgets
        // from leaking the polling thread too.
        self.shutdown();
    }
}

fn do_scan(inner: &Inner) -> ScanSummary {
    let _serialized = lock(&inner.scan_lock);
    let new_snapshot = build_snapshot(&inner.config);
    let mut g = lock(&inner.state);
    let raws = diff_snapshots(&g.snapshot, &new_snapshot);
    let raw_count = raws.len();
    g.snapshot = new_snapshot;
    g.scans += 1;
    if !raws.is_empty() {
        g.cache_epoch += 1;
        for raw in &raws {
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
        for raw in &raws {
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
    ScanSummary {
        files_tracked: g.snapshot.len(),
        raw_events: raw_count,
    }
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
            "{{\"roots\":[{}],\"poll_interval_ms\":{},\"files_tracked\":{},\"events_emitted\":{},\"queue_len\":{},\"dropped\":{},\"next_seq\":{},\"scans\":{},\"cache_entries\":{},\"cache_hits\":{},\"cache_misses\":{},\"invalidations\":{}}}",
            roots.join(","),
            self.poll_interval_ms,
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
