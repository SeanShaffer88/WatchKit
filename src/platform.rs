//! Platform layer: the native change-discovery backend.
//!
//! The portable core (`src/lib.rs`) polls. This module is
//! the v0.2 alternative on Windows: one thread per watcher
//! running `ReadDirectoryChangesW` (kernel32, hand-declared
//! — zero dependencies) over every root, feeding the SAME
//! snapshot/queue/cache pipeline the poller uses through
//! `crate::apply_observed_changes` and
//! `crate::do_root_rescan`. The coalescing table, queue
//! bounds, and cache-invalidation rules do not live here
//! and do not change here; this layer only discovers raw
//! changes sooner.
//!
//! Honesty rules, same as the rest of the family: the
//! backend reports as running only when it actually
//! started. `try_start_native` returns `None` off Windows
//! and on any startup failure, and the caller falls back
//! to polling and reports polling. If the native thread
//! dies unexpectedly at runtime, it flips the reported
//! backend to polling and spawns the polling thread as a
//! takeover before exiting (`crate::native_failed_takeover`).

use crate::Inner;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// FILE_NOTIFY_INFORMATION parsing (pure, all platforms)
// ---------------------------------------------------------------------------

/// The `FILE_NOTIFY_CHANGE_*` action codes, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyAction {
    Added,
    Removed,
    Modified,
    RenamedOldName,
    RenamedNewName,
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// Parse one `ReadDirectoryChangesW` result buffer — a
/// chain of `FILE_NOTIFY_INFORMATION` records
/// (`NextEntryOffset` / `Action` / `FileNameLength` /
/// UTF-16LE name) — into `(action, relative path)` pairs.
/// Names come back with `/` separators (Windows reports
/// `\`; the rest of the kit speaks `/` in relative paths).
///
/// Malformed data is an error, never a panic and never a
/// guess: a truncated header or name, an odd name length,
/// an unknown action, or a `NextEntryOffset` that does
/// not strictly advance within the buffer all return
/// `Err`. The caller treats a parse error like an
/// overflow (one full rescan of the root), so a corrupt
/// buffer degrades to a walk, not to lost changes. A
/// record with a zero-length name is skipped — it names
/// nothing the snapshot could hold.
pub fn parse_notify_buffer(buf: &[u8]) -> Result<Vec<(NotifyAction, String)>, String> {
    let mut out = Vec::new();
    let mut offset: usize = 0;
    while offset < buf.len() {
        let remaining = buf.len() - offset;
        if remaining < 12 {
            return Err(format!(
                "truncated FILE_NOTIFY_INFORMATION header: {remaining} byte(s) left"
            ));
        }
        let next = read_u32(buf, offset) as usize;
        let action_raw = read_u32(buf, offset + 4);
        let name_len = read_u32(buf, offset + 8) as usize;
        if name_len > remaining - 12 {
            return Err(format!(
                "truncated FILE_NOTIFY_INFORMATION name: {name_len} byte(s) claimed, {} available",
                remaining - 12
            ));
        }
        if name_len % 2 != 0 {
            return Err(format!("odd FileNameLength {name_len}"));
        }
        let action = match action_raw {
            0x0000_0001 => NotifyAction::Added,
            0x0000_0002 => NotifyAction::Removed,
            0x0000_0003 => NotifyAction::Modified,
            0x0000_0004 => NotifyAction::RenamedOldName,
            0x0000_0005 => NotifyAction::RenamedNewName,
            other => return Err(format!("unknown FILE_NOTIFY_CHANGE action {other}")),
        };
        if name_len > 0 {
            let bytes = &buf[offset + 12..offset + 12 + name_len];
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let name = String::from_utf16_lossy(&units).replace('\\', "/");
            out.push((action, name));
        }
        if next == 0 {
            break;
        }
        if next < 12 {
            return Err(format!(
                "NextEntryOffset {next} does not advance past a record header"
            ));
        }
        if offset + next > buf.len() {
            return Err(format!(
                "NextEntryOffset {next} runs past the buffer ({} byte(s))",
                buf.len()
            ));
        }
        offset += next;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Native backend handle
// ---------------------------------------------------------------------------

/// Owns the native thread's stop event and join handle.
/// Constructed only by a successful Windows startup; on
/// other platforms the struct is empty and
/// `try_start_native` never hands one out.
pub struct NativeHandle {
    #[cfg(windows)]
    stop_event: isize,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NativeHandle {
    /// Signal the native thread to stop and join it. The
    /// thread drains its overlapped I/O and closes its
    /// directory handles before exiting, so when this
    /// returns nothing of the backend is left running.
    pub fn stop_and_join(&mut self) {
        #[cfg(windows)]
        {
            if self.stop_event != 0 {
                win::set_event(self.stop_event);
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            if self.stop_event != 0 {
                win::close_handle(self.stop_event);
                self.stop_event = 0;
            }
        }
    }
}

/// Try to start the native backend for `inner`. Returns
/// `Some(handle)` only when the backend is genuinely
/// running: on Windows, after every root was opened and
/// the watch thread spawned. Any failure — wrong
/// platform, a root that will not open, too many roots
/// for one wait — returns `None` and the caller falls
/// back to (and reports) polling.
pub(crate) fn try_start_native(inner: Arc<Inner>) -> Option<NativeHandle> {
    #[cfg(windows)]
    {
        win::start(inner).ok()
    }
    #[cfg(not(windows))]
    {
        let _ = inner;
        None
    }
}

// ---------------------------------------------------------------------------
// Windows: the ReadDirectoryChangesW thread
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::*;
    use crate::{SnapshotEntry, ERR_PLATFORM};
    use std::os::raw::c_void;

    // Access / sharing / creation / flags for CreateFileW.
    const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    const INVALID_HANDLE_VALUE: isize = -1;
    // ReadDirectoryChangesW notify filter: file name, dir
    // name, attributes, size, last write — the signals the
    // snapshot diff itself uses (size + mtime) plus names.
    const NOTIFY_FILTER: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004 | 0x0000_0008 | 0x0000_0010;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_FAILED: u32 = 0xFFFF_FFFF;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const MAXIMUM_WAIT_OBJECTS: usize = 64;
    /// Per-root completion buffer. A full buffer (or an
    /// overflow completion) degrades to a rescan of that
    /// root, so 64 KiB is headroom, not a correctness
    /// bound.
    const BUFFER_BYTES: usize = 64 * 1024;

    /// OVERLAPPED, x86-64 layout: Internal, InternalHigh,
    /// the Offset/Pointer union (8 bytes), hEvent.
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: isize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: isize,
        ) -> isize;
        fn CreateEventW(
            event_attributes: *mut c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> isize;
        fn SetEvent(event: isize) -> i32;
        fn ResetEvent(event: isize) -> i32;
        fn CloseHandle(object: isize) -> i32;
        fn ReadDirectoryChangesW(
            directory: isize,
            buffer: *mut u8,
            buffer_length: u32,
            watch_subtree: i32,
            notify_filter: u32,
            bytes_returned: *mut u32,
            overlapped: *mut Overlapped,
            completion_routine: *mut c_void,
        ) -> i32;
        fn GetOverlappedResult(
            file: isize,
            overlapped: *mut Overlapped,
            bytes_transferred: *mut u32,
            wait: i32,
        ) -> i32;
        fn CancelIoEx(file: isize, overlapped: *mut Overlapped) -> i32;
        fn WaitForMultipleObjects(count: u32, handles: *const isize, wait_all: i32, ms: u32)
            -> u32;
        fn GetLastError() -> u32;
    }

    pub(crate) fn set_event(h: isize) {
        // SAFETY: `h` is the stop event created in `start`
        // and still owned by the caller's NativeHandle.
        unsafe {
            SetEvent(h);
        }
    }

    pub(crate) fn close_handle(h: isize) {
        // SAFETY: `h` is a kernel handle owned by the
        // caller, closed exactly once (the handle field is
        // zeroed after this call by NativeHandle).
        unsafe {
            CloseHandle(h);
        }
    }

    fn last_error() -> u32 {
        // SAFETY: GetLastError takes no arguments.
        unsafe { GetLastError() }
    }

    fn platform_err(what: &str) -> (i32, String) {
        (
            ERR_PLATFORM,
            format!("{what} failed (GetLastError={})", last_error()),
        )
    }

    /// One watched root: directory handle, completion
    /// event, its OVERLAPPED (boxed — its address must not
    /// move while I/O is pending) and its result buffer.
    struct RootWatch {
        root: String,
        dir: isize,
        event: isize,
        overlapped: Box<Overlapped>,
        buffer: Vec<u8>,
        pending: bool,
    }

    impl RootWatch {
        fn open(root: &str) -> Result<RootWatch, (i32, String)> {
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<u16> = std::ffi::OsStr::new(root)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `wide` is a NUL-terminated UTF-16
            // path that outlives the call; the returned
            // handle is owned by the RootWatch and closed
            // in cleanup exactly once.
            let dir = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    0,
                )
            };
            if dir == INVALID_HANDLE_VALUE {
                return Err(platform_err(&format!("CreateFileW on root \"{root}\"")));
            }
            // SAFETY: NULL attributes/name, manual-reset,
            // initially non-signalled — a plain event.
            let event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
            if event == 0 {
                let e = platform_err("CreateEventW for a root watch");
                // SAFETY: `dir` was just opened above and
                // is not used again on this path.
                unsafe {
                    CloseHandle(dir);
                }
                return Err(e);
            }
            Ok(RootWatch {
                root: root.to_string(),
                dir,
                event,
                overlapped: Box::new(Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    h_event: event,
                }),
                buffer: vec![0; BUFFER_BYTES],
                pending: false,
            })
        }

        /// Issue (or re-issue) the overlapped
        /// ReadDirectoryChangesW for this root. Roots are
        /// recursive by the kit's contract, so the subtree
        /// is always watched.
        fn issue(&mut self) -> Result<(), (i32, String)> {
            let mut returned: u32 = 0;
            // SAFETY: the buffer and OVERLAPPED are owned
            // by `self`, which outlives the pending
            // operation (cleanup drains it before either
            // is dropped); no completion routine is used.
            let ok = unsafe {
                ReadDirectoryChangesW(
                    self.dir,
                    self.buffer.as_mut_ptr(),
                    self.buffer.len() as u32,
                    1,
                    NOTIFY_FILTER,
                    &mut returned,
                    &mut *self.overlapped,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(platform_err("ReadDirectoryChangesW"));
            }
            self.pending = true;
            Ok(())
        }

        /// Cancel any pending operation, drain it, and
        /// close both handles. Runs on the watch thread as
        /// its last act, so no other thread touches these
        /// handles afterwards.
        fn cleanup(&mut self) {
            // SAFETY: the handles are owned by `self`; the
            // drain (blocking GetOverlappedResult after
            // CancelIoEx) guarantees the kernel is done
            // with the buffer and OVERLAPPED before they
            // are dropped by the caller.
            unsafe {
                if self.pending {
                    CancelIoEx(self.dir, &mut *self.overlapped);
                    let mut transferred: u32 = 0;
                    GetOverlappedResult(self.dir, &mut *self.overlapped, &mut transferred, 1);
                    self.pending = false;
                }
                if self.event != 0 {
                    CloseHandle(self.event);
                    self.event = 0;
                }
                if self.dir != INVALID_HANDLE_VALUE && self.dir != 0 {
                    CloseHandle(self.dir);
                    self.dir = 0;
                }
            }
        }
    }

    /// Open every root, issue the first read on each, and
    /// spawn the single watch thread. Any failure closes
    /// whatever was opened and reports `Err` — the caller
    /// then falls back to polling.
    pub fn start(inner: Arc<Inner>) -> Result<NativeHandle, (i32, String)> {
        if inner.config.roots.len() + 1 > MAXIMUM_WAIT_OBJECTS {
            return Err((
                ERR_PLATFORM,
                format!(
                    "{} roots exceed the {}-object wait budget of one watch thread",
                    inner.config.roots.len(),
                    MAXIMUM_WAIT_OBJECTS - 1
                ),
            ));
        }
        // SAFETY: NULL attributes/name, manual-reset,
        // initially non-signalled — the stop event.
        let stop_event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        if stop_event == 0 {
            return Err(platform_err("CreateEventW for the stop event"));
        }
        let mut watches: Vec<RootWatch> = Vec::new();
        let mut setup = || -> Result<(), (i32, String)> {
            for root in &inner.config.roots {
                let mut w = RootWatch::open(root)?;
                w.issue()?;
                watches.push(w);
            }
            Ok(())
        };
        if let Err(e) = setup() {
            for w in &mut watches {
                w.cleanup();
            }
            // SAFETY: `stop_event` was created above and
            // has not been handed to anyone yet.
            unsafe {
                CloseHandle(stop_event);
            }
            return Err(e);
        }
        let thread_inner = inner.clone();
        let thread = std::thread::spawn(move || run(thread_inner, watches, stop_event));
        Ok(NativeHandle {
            stop_event,
            thread: Some(thread),
        })
    }

    /// The watch thread: wait on the stop event plus one
    /// completion event per root; for each completion,
    /// parse + apply or rescan, then re-issue. Exits only
    /// via the stop event or an unexpected failure (which
    /// hands discovery over to the polling takeover first).
    fn run(inner: Arc<Inner>, mut watches: Vec<RootWatch>, stop_event: isize) {
        let mut handles: Vec<isize> = Vec::with_capacity(watches.len() + 1);
        handles.push(stop_event);
        for w in &watches {
            handles.push(w.event);
        }
        let mut failed = false;
        'wait: loop {
            // SAFETY: `handles` holds the stop event plus
            // one live event per watch, all owned for the
            // whole wait; count fits the wait budget
            // (checked in `start`).
            let res = unsafe {
                WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE)
            };
            if res == WAIT_OBJECT_0 {
                break 'wait; // stop requested
            }
            let idx = (res.wrapping_sub(WAIT_OBJECT_0)) as usize;
            if res == WAIT_FAILED || idx == 0 || idx > watches.len() {
                failed = true;
                break 'wait;
            }
            let w = &mut watches[idx - 1];
            let mut transferred: u32 = 0;
            // SAFETY: the watch's OVERLAPPED is boxed and
            // stable; its completion event is what woke
            // the wait, so a non-blocking drain is correct.
            let ok = unsafe { GetOverlappedResult(w.dir, &mut *w.overlapped, &mut transferred, 0) };
            w.pending = false;
            let root = w.root.clone();
            if ok != 0 && transferred > 0 {
                let bytes = w.buffer[..transferred as usize].to_vec();
                match parse_notify_buffer(&bytes) {
                    Ok(changes) => observe(&inner, &root, changes),
                    // A malformed buffer is treated like an
                    // overflow: whatever it held is
                    // rediscovered by a full rescan.
                    Err(_) => {
                        crate::do_root_rescan(&inner, &root);
                    }
                }
            } else {
                // A completion with 0 bytes transferred,
                // and a failed completion carrying
                // ERROR_NOTIFY_ENUM_DIR (866), are the two
                // documented overflow signals; any other
                // completion failure loses exactly the same
                // ground. Every case converges on one
                // rescan of this root through the polling
                // machinery.
                crate::do_root_rescan(&inner, &root);
            }
            // SAFETY: the event is manual-reset and owned
            // by the watch; it must be reset before the
            // operation is re-issued on it.
            unsafe {
                ResetEvent(w.event);
            }
            if w.issue().is_err() {
                failed = true;
                break 'wait;
            }
        }
        for w in &mut watches {
            w.cleanup();
        }
        if failed {
            // Discovery must not just stop: report polling
            // and hand over to a polling thread (unless the
            // watcher is already shutting down, in which
            // case wa_close owns the threads).
            crate::native_failed_takeover(&inner);
        }
    }

    /// Turn one root's parsed changes into observed
    /// snapshot state and apply them through the shared
    /// pipeline. The action mapping is realized by
    /// stat-ing each named path: a path that exists now is
    /// upserted with its fresh entry (ADDED /
    /// RENAMED_NEW_NAME / MODIFIED); a path that is gone
    /// is a removal (REMOVED / RENAMED_OLD_NAME). Ignore
    /// globs are applied here, exactly as the poller's
    /// walk applies them, so an ignored path is never
    /// tracked by either backend.
    fn observe(inner: &Inner, root: &str, changes: Vec<(NotifyAction, String)>) {
        let mut observed: Vec<(String, Option<SnapshotEntry>)> = Vec::new();
        for (_action, rel) in &changes {
            if rel.is_empty() || crate::is_ignored(rel, &inner.config.ignore) {
                continue;
            }
            // Join component by component so the snapshot
            // key's separators come out exactly as the
            // polling walk's read_dir paths spell them.
            let mut path = std::path::PathBuf::from(root);
            for comp in rel.split('/') {
                if !comp.is_empty() {
                    path.push(comp);
                }
            }
            let key = path.to_string_lossy().into_owned();
            let entry = std::fs::symlink_metadata(&path).ok().map(|md| {
                let ft = md.file_type();
                SnapshotEntry {
                    size: if ft.is_dir() { 0 } else { md.len() },
                    mtime_nanos: crate::mtime_nanos_of(&md),
                    is_dir: ft.is_dir(),
                }
            });
            observed.push((key, entry));
        }
        crate::apply_observed_changes(inner, &observed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one FILE_NOTIFY_INFORMATION record: next
    /// offset, action, UTF-16LE name.
    fn rec(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = Vec::new();
        v.extend_from_slice(&next.to_le_bytes());
        v.extend_from_slice(&action.to_le_bytes());
        v.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_a_single_record() {
        let buf = rec(0, 1, "a.txt");
        let out = parse_notify_buffer(&buf).unwrap();
        assert_eq!(out, vec![(NotifyAction::Added, "a.txt".to_string())]);
    }

    #[test]
    fn parses_a_multi_record_chain() {
        let r1 = rec(20, 1, "one"); // 12 + 3*2 = 18, padded by the next field layout to 20 via explicit offset
        let mut buf = Vec::new();
        // Record 1 claims a 2-byte pad after its name.
        let mut first = r1.clone();
        first.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&first);
        buf.extend_from_slice(&rec(28, 4, "old name"));
        buf.extend_from_slice(&rec(0, 5, "new name"));
        let out = parse_notify_buffer(&buf).unwrap();
        assert_eq!(
            out,
            vec![
                (NotifyAction::Added, "one".to_string()),
                (NotifyAction::RenamedOldName, "old name".to_string()),
                (NotifyAction::RenamedNewName, "new name".to_string()),
            ]
        );
    }

    #[test]
    fn backslashes_become_slashes() {
        let buf = rec(0, 3, "sub\\deep\\file.txt");
        let out = parse_notify_buffer(&buf).unwrap();
        assert_eq!(
            out,
            vec![(NotifyAction::Modified, "sub/deep/file.txt".to_string())]
        );
    }

    #[test]
    fn zero_length_name_is_skipped_not_fatal() {
        let mut buf = rec(12, 2, "");
        buf.extend_from_slice(&rec(0, 1, "real.txt"));
        let out = parse_notify_buffer(&buf).unwrap();
        assert_eq!(out, vec![(NotifyAction::Added, "real.txt".to_string())]);
    }

    #[test]
    fn truncated_final_record_is_an_error() {
        let mut buf = rec(0, 1, "a-longer-name.txt");
        let cut = buf.len() - 3;
        buf.truncate(cut);
        assert!(parse_notify_buffer(&buf).is_err());
    }

    #[test]
    fn truncated_header_is_an_error() {
        let buf = vec![1u8, 0, 0, 0, 2];
        assert!(parse_notify_buffer(&buf).is_err());
    }

    #[test]
    fn next_offset_past_the_buffer_is_an_error() {
        let buf = rec(4096, 1, "x");
        assert!(parse_notify_buffer(&buf).is_err());
    }

    #[test]
    fn unknown_action_is_an_error() {
        let buf = rec(0, 99, "x");
        assert!(parse_notify_buffer(&buf).is_err());
    }

    #[test]
    fn empty_buffer_is_no_changes() {
        assert_eq!(parse_notify_buffer(&[]).unwrap(), Vec::new());
    }

    #[test]
    fn native_backend_is_unavailable_off_windows() {
        // try_start_native returning None off Windows is
        // what makes "auto"/"native" fall back to polling
        // there — the fallback the status field reports.
        #[cfg(not(windows))]
        {
            let root = std::env::temp_dir().to_string_lossy().into_owned();
            let cfg = crate::parse_config(&format!(
                "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0,\"backend\":\"native\"}}",
                crate::json_escape(&root)
            ))
            .unwrap();
            let w = crate::Watcher::create(cfg).unwrap();
            assert!(try_start_native(w.inner.clone()).is_none());
            assert_eq!(
                crate::running_backend(&w.inner),
                crate::RunningBackend::Polling
            );
        }
    }
}
