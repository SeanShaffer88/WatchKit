//! C ABI surface — everything declared in include/watchkit.h.
//! Conventions (house standard):
//! - `*mut c_char` results are caller-owned; free with
//!   `wa_free_string`.
//! - Failures set a thread-local last-error (code +
//!   message) and return the documented sentinel (a WA_*
//!   code, or a null handle from wa_create).
//! - Nothing unwinds across the FFI boundary (catch_unwind
//!   everywhere; release profile also sets panic = "abort").
//! - A watcher handle IS thread-safe: all state is
//!   serialized internally, so polls, cache queries and
//!   controls may come from any thread.

// The exported functions ARE the raw-pointer C ABI declared
// in include/watchkit.h; pointer validity is the documented
// contract.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::catch_unwind;
use std::sync::Arc;

use crate::{parse_config, WaResult, Watcher, ERR_INTERNAL, ERR_INVALID_ARG, WA_OK};

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(WA_OK, "");
}

fn fail(code: i32, msg: &str) -> c_int {
    set_error(code, msg);
    code as c_int
}

fn panic_code() -> c_int {
    set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
    ERR_INTERNAL as c_int
}

fn guard<T, F: FnOnce() -> T>(f: F) -> std::thread::Result<T> {
    catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> WaResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

fn watcher_ref<'a>(ptr: *mut Watcher) -> WaResult<&'a Watcher> {
    if ptr.is_null() {
        return Err((
            ERR_INVALID_ARG,
            "watcher handle must not be null".to_string(),
        ));
    }
    Ok(unsafe { &*ptr })
}

fn out_string(out: *mut *mut c_char, text: String) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_json must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    match CString::new(text) {
        Ok(s) => {
            unsafe { *out = s.into_raw() };
            clear_error();
            WA_OK as c_int
        }
        Err(_) => fail(ERR_INTERNAL, "result string contained a NUL byte"),
    }
}

fn json_call<F: FnOnce(&Watcher) -> WaResult<String>>(
    handle: *mut Watcher,
    out_json: *mut *mut c_char,
    f: F,
) -> c_int {
    match guard(|| {
        if out_json.is_null() {
            return fail(ERR_INVALID_ARG, "out_json must not be null");
        }
        unsafe { *out_json = std::ptr::null_mut() };
        let inner = watcher_ref(handle).and_then(f);
        match inner {
            Ok(text) => out_string(out_json, text),
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn wa_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn wa_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn wa_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string this
/// library returned and has not been freed yet. Documented
/// in the header.
#[no_mangle]
pub extern "C" fn wa_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

fn create_impl(config_json: *const c_char, out: *mut *mut Watcher) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_watcher must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    let inner = (|| -> WaResult<Arc<Watcher>> {
        let text = cstr(config_json, "config_json")?;
        let cfg = parse_config(text)?;
        Watcher::create(cfg)
    })();
    match inner {
        Ok(w) => {
            unsafe { *out = Arc::into_raw(w) as *mut Watcher };
            clear_error();
            WA_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn wa_create(config_json: *const c_char, out_watcher: *mut *mut Watcher) -> c_int {
    match guard(|| create_impl(config_json, out_watcher)) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn wa_scan_once(watcher: *mut Watcher) -> c_int {
    match guard(|| match watcher_ref(watcher) {
        Ok(w) => {
            w.scan_once();
            clear_error();
            WA_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn wa_poll_events(
    watcher: *mut Watcher,
    since_seq: u64,
    max_events: u32,
    out_json: *mut *mut c_char,
) -> c_int {
    json_call(watcher, out_json, |w| {
        Ok(w.poll_events(since_seq, max_events).to_json())
    })
}

#[no_mangle]
pub extern "C" fn wa_folder_size(
    watcher: *mut Watcher,
    path: *const c_char,
    out_json: *mut *mut c_char,
) -> c_int {
    // Resolve the path argument first (borrow tied to the
    // call, not to any handle state), then serve.
    let path = match cstr(path, "path") {
        Ok(p) => p.to_string(),
        Err((code, msg)) => return fail(code, &msg),
    };
    json_call(watcher, out_json, |w| {
        w.folder_size(&path).map(|r| r.to_json())
    })
}

#[no_mangle]
pub extern "C" fn wa_cached_search(
    watcher: *mut Watcher,
    root: *const c_char,
    substring: *const c_char,
    out_json: *mut *mut c_char,
) -> c_int {
    let root = match cstr(root, "root") {
        Ok(p) => p.to_string(),
        Err((code, msg)) => return fail(code, &msg),
    };
    let substring = match cstr(substring, "substring") {
        Ok(p) => p.to_string(),
        Err((code, msg)) => return fail(code, &msg),
    };
    json_call(watcher, out_json, |w| {
        w.cached_search(&root, &substring).map(|r| r.to_json())
    })
}

#[no_mangle]
pub extern "C" fn wa_status(watcher: *mut Watcher, out_json: *mut *mut c_char) -> c_int {
    json_call(watcher, out_json, |w| Ok(w.status().to_json()))
}

/// Close the handle and free it. The background polling
/// thread is signalled and JOINED before this returns —
/// a handle never leaks a thread. Null is a no-op.
/// Anything through the handle after this is a contract
/// violation (use-after-close).
#[no_mangle]
pub extern "C" fn wa_close(watcher: *mut Watcher) {
    if !watcher.is_null() {
        let _ = guard(|| unsafe {
            let arc = Arc::from_raw(watcher as *const Watcher);
            arc.shutdown();
            // `arc` drops here; shutdown joined the
            // polling thread, so the watcher is freed
            // exactly once with no thread left behind.
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn take_string(p: *mut c_char) -> String {
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        wa_free_string(p);
        s
    }

    fn call_json<F: Fn(*mut *mut c_char) -> c_int>(f: F) -> crate::json::Value {
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(f(&mut out), WA_OK as c_int);
        crate::json::parse(&take_string(out)).unwrap()
    }

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("watchkit-ffi-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }
        fn s(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn create_in(dir: &Tmp) -> *mut Watcher {
        let cfg = cs(&format!(
            "{{\"roots\":[\"{}\"],\"poll_interval_ms\":0}}",
            crate::json_escape(&dir.s())
        ));
        let mut w: *mut Watcher = std::ptr::null_mut();
        assert_eq!(wa_create(cfg.as_ptr(), &mut w), WA_OK as c_int);
        assert!(!w.is_null());
        w
    }

    #[test]
    fn ffi_end_to_end() {
        let d = Tmp::new();
        let w = create_in(&d);

        // Create a file, force a scan, poll the event.
        std::fs::write(d.0.join("hello.txt"), b"hello").unwrap();
        assert_eq!(wa_scan_once(w), WA_OK as c_int);
        let poll = call_json(|out| wa_poll_events(w, 0, 0, out));
        let events = poll.get("events").unwrap().as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].get("type").unwrap().as_str(), Some("created"));
        assert_eq!(events[0].get("seq").unwrap().as_i64(), Some(1));
        assert_eq!(poll.get("next_seq").unwrap().as_i64(), Some(1));
        assert_eq!(poll.get("dropped").unwrap().as_i64(), Some(0));

        // Status + folder size + search, all as JSON.
        let st = call_json(|out| wa_status(w, out));
        assert_eq!(st.get("files_tracked").unwrap().as_i64(), Some(1));
        assert_eq!(st.get("events_emitted").unwrap().as_i64(), Some(1));
        let root = cs(&d.s());
        let size = call_json(|out| wa_folder_size(w, root.as_ptr(), out));
        assert_eq!(size.get("total_bytes").unwrap().as_i64(), Some(5));
        assert_eq!(size.get("file_count").unwrap().as_i64(), Some(1));
        assert_eq!(size.get("cached").unwrap().as_bool(), Some(false));
        let size2 = call_json(|out| wa_folder_size(w, root.as_ptr(), out));
        assert_eq!(size2.get("cached").unwrap().as_bool(), Some(true));
        let search = call_json(|out| wa_cached_search(w, root.as_ptr(), cs("HELLO").as_ptr(), out));
        assert_eq!(search.get("count").unwrap().as_i64(), Some(1));

        wa_close(w);
    }

    #[test]
    fn ffi_config_and_handle_errors() {
        let mut w: *mut Watcher = std::ptr::null_mut();
        // Malformed JSON -> WA_ERR_PARSE, no handle.
        assert_eq!(
            wa_create(cs("{\"roots\":").as_ptr(), &mut w),
            crate::ERR_PARSE as c_int
        );
        assert!(w.is_null());
        // Missing roots -> WA_ERR_INVALID_ARG.
        assert_eq!(
            wa_create(cs("{}").as_ptr(), &mut w),
            ERR_INVALID_ARG as c_int
        );
        // Nonexistent root -> WA_ERR_NOT_FOUND.
        assert_eq!(
            wa_create(
                cs("{\"roots\":[\"/watchkit-definitely-missing-xyz\"]}").as_ptr(),
                &mut w
            ),
            crate::ERR_NOT_FOUND as c_int
        );
        // Null config / null handle.
        assert_eq!(
            wa_create(std::ptr::null(), &mut w),
            ERR_INVALID_ARG as c_int
        );
        assert_eq!(wa_last_error_code(), ERR_INVALID_ARG as c_int);
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            wa_status(std::ptr::null_mut(), &mut out),
            ERR_INVALID_ARG as c_int
        );
        assert_eq!(wa_scan_once(std::ptr::null_mut()), ERR_INVALID_ARG as c_int);
        assert_eq!(
            wa_folder_size(std::ptr::null_mut(), cs("x").as_ptr(), &mut out),
            ERR_INVALID_ARG as c_int
        );
        wa_close(std::ptr::null_mut()); // no-op, must not crash
    }
}
