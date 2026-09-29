//! Progress reporting for network operations, handed to Swift per call.
//!
//! grit already models this: `http_fetch` and `push_http` take a
//! `&mut dyn Progress`, and the shim was passing `NoProgress`, so every update
//! went nowhere. The split below is between what grit reports live and what it
//! only hands back at the end.
//!
//! **Live, during the transfer** -- grit's sideband channel:
//! `sideband`. This is git's own "remote: Counting objects: 47%" text.
//!
//! **Real numbers, emitted at the end** from data grit already returns:
//! `update_tip` from each `RefUpdate`, `push_update_ref` from each
//! `PushRefResult`, `push_negotiation` from the refspecs we are about to send,
//! and the byte totals in `transfer` / `push_transfer`, which the HTTP client
//! counts as bodies cross the boundary.
//!
//! **Phase markers only** -- `pack`: grit builds the pack inside `push_http`
//! and never reports object counts while it does, so this carries the phase
//! and zeroes rather than an invented percentage.
//!
//! A count that grit does not know is passed as 0, never as a guess. The Swift
//! side is expected to render byte totals and phase names, and to leave object
//! counts out of the UI when they are 0.

use std::os::raw::{c_char, c_void};

/// A sideband line from the remote.
pub type RawSidebandFn = extern "C" fn(context: *mut c_void, text: *const c_char);
/// Receiving: `received_bytes` is real; the object counts are what grit
/// reported, and are 0 when it reported nothing.
pub type RawTransferFn = extern "C" fn(
    context: *mut c_void,
    total_objects: u32,
    indexed_objects: u32,
    received_objects: u32,
    local_objects: u32,
    total_deltas: u32,
    indexed_deltas: u32,
    received_bytes: u64,
);
/// A ref moved locally. Either oid may be NULL when the ref was created or
/// deleted.
pub type RawUpdateTipFn = extern "C" fn(
    context: *mut c_void,
    refname: *const c_char,
    old_oid: *const c_char,
    new_oid: *const c_char,
);
/// A pack-building phase marker. `stage` 0 is building, 1 is uploading.
pub type RawPackFn = extern "C" fn(context: *mut c_void, stage: i32, current: u32, total: u32);
/// Uploading: `bytes` is the number of request-body bytes sent.
pub type RawPushTransferFn =
    extern "C" fn(context: *mut c_void, current: u32, total: u32, bytes: u64);
/// One ref's verdict from the server. `status` is NULL when accepted.
pub type RawPushUpdateRefFn =
    extern "C" fn(context: *mut c_void, refname: *const c_char, status: *const c_char);
/// The updates about to be sent. `refname` is the destination, `new_oid` the
/// local tip; `old_oid` is NULL because the server's value is not known until
/// the advertisement comes back.
pub type RawPushNegotiationFn =
    extern "C" fn(context: *mut c_void, refname: *const c_char, new_oid: *const c_char);

/// The seven progress hooks, passed **per call** alongside `HttpCallbacks`.
///
/// Any of them may be NULL. They are grouped in one `#[repr(C)]` struct for the
/// same reason the HTTP ones are: a set of separate globals can be observed
/// half-updated, and a partial set is exactly the bug that was just fixed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProgressCallbacks {
    pub sideband: Option<RawSidebandFn>,
    pub transfer: Option<RawTransferFn>,
    pub update_tip: Option<RawUpdateTipFn>,
    pub pack: Option<RawPackFn>,
    pub push_transfer: Option<RawPushTransferFn>,
    pub push_update_ref: Option<RawPushUpdateRefFn>,
    pub push_negotiation: Option<RawPushNegotiationFn>,
    /// Opaque embedder state, handed back as the first argument of every hook.
    ///
    /// The C function pointers cannot capture anything, so this is the only way
    /// for a Swift sink to be reachable at all. It travels with the struct
    /// because the struct is passed by value, which is also what makes it safe:
    /// grit never outlives the call that was given the struct.
    pub context: *mut c_void,
}

impl Default for ProgressCallbacks {
    fn default() -> Self {
        Self {
            sideband: None,
            transfer: None,
            update_tip: None,
            pack: None,
            push_transfer: None,
            push_update_ref: None,
            push_negotiation: None,
            context: std::ptr::null_mut(),
        }
    }
}

impl ProgressCallbacks {
    /// The set for a host build with no Swift, and for tests that do not care.
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether anything at all is listening.
    pub fn is_empty(&self) -> bool {
        self.sideband.is_none()
            && self.transfer.is_none()
            && self.update_tip.is_none()
            && self.pack.is_none()
            && self.push_transfer.is_none()
            && self.push_update_ref.is_none()
            && self.push_negotiation.is_none()
    }
}

/// Adapts the C callbacks to grit's [`grit_lib::fetch::Progress`].
///
/// grit calls `message` from deep inside the packet loop, so this must stay
/// cheap and must never allocate on a failure path: a panic across an FFI
/// boundary is undefined behaviour.
pub struct ProgressBridge {
    callbacks: ProgressCallbacks,
}

impl ProgressBridge {
    pub fn new(callbacks: ProgressCallbacks) -> Self {
        Self { callbacks }
    }
}

impl grit_lib::fetch::Progress for ProgressBridge {
    fn message(&mut self, bytes: &[u8]) {
        let Some(f) = self.callbacks.sideband else {
            return;
        };
        // The text is not guaranteed to be UTF-8 on the wire; a sideband line
        // that is not text should not take the transfer down.
        let Ok(text) = std::str::from_utf8(bytes) else {
            return;
        };
        if let Ok(c) = std::ffi::CString::new(text) {
            f(self.callbacks.context, c.as_ptr());
        }
    }
}

/// The emitter half: called at the points grit hands us data back.
///
/// Kept separate from `ProgressBridge` because these fire on the shim's own
/// schedule, not grit's.
#[derive(Clone, Copy)]
pub struct ProgressReporter {
    callbacks: ProgressCallbacks,
}

impl ProgressReporter {
    pub fn new(callbacks: ProgressCallbacks) -> Self {
        Self { callbacks }
    }

    /// A sideband line, for phases grit announces through its own progress
    /// channel rather than the remote's.
    pub fn sideband(&self, text: &str) {
        let Some(f) = self.callbacks.sideband else {
            return;
        };
        if let Ok(c) = std::ffi::CString::new(text) {
            f(self.callbacks.context, c.as_ptr());
        }
    }

    /// A ref moved locally.
    pub fn update_tip(&self, refname: &str, old_oid: Option<&str>, new_oid: Option<&str>) {
        let Some(f) = self.callbacks.update_tip else {
            return;
        };
        let Ok(name) = std::ffi::CString::new(refname) else {
            return;
        };
        let old = old_oid.and_then(|o| std::ffi::CString::new(o).ok());
        let new = new_oid.and_then(|o| std::ffi::CString::new(o).ok());
        f(
            self.callbacks.context,
            name.as_ptr(),
            old.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            new.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        );
    }

    /// Receiving. `received_bytes` is measured at the HTTP boundary; the object
    /// counts are 0 when grit did not report them.
    pub fn transfer(&self, received_objects: u32, received_bytes: u64) {
        let Some(f) = self.callbacks.transfer else {
            return;
        };
        f(
            self.callbacks.context,
            0,
            0,
            received_objects,
            0,
            0,
            0,
            received_bytes,
        );
    }

    /// A pack phase. `stage` 0 is building, 1 is uploading.
    pub fn pack(&self, stage: i32) {
        let Some(f) = self.callbacks.pack else { return };
        f(self.callbacks.context, stage, 0, 0);
    }

    /// Uploading. `bytes` is the request-body bytes actually sent.
    pub fn push_transfer(&self, bytes: u64) {
        let Some(f) = self.callbacks.push_transfer else {
            return;
        };
        f(self.callbacks.context, 0, 0, bytes);
    }

    /// One ref's verdict. `status` is None when the server accepted it.
    pub fn push_update_ref(&self, refname: &str, status: Option<&str>) {
        let Some(f) = self.callbacks.push_update_ref else {
            return;
        };
        let Ok(name) = std::ffi::CString::new(refname) else {
            return;
        };
        let status = status.and_then(|s| std::ffi::CString::new(s).ok());
        f(
            self.callbacks.context,
            name.as_ptr(),
            status.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        );
    }

    /// The update about to be sent.
    pub fn push_negotiation(&self, refname: &str, new_oid: &str) {
        let Some(f) = self.callbacks.push_negotiation else {
            return;
        };
        let Ok(name) = std::ffi::CString::new(refname) else {
            return;
        };
        let Ok(new) = std::ffi::CString::new(new_oid) else {
            return;
        };
        f(self.callbacks.context, name.as_ptr(), new.as_ptr());
    }
}

/// Bytes crossing the HTTP boundary, shared between the client and the
/// reporter so the reporter can say how much actually moved.
///
/// `HttpClient::get`/`post` take `&self`, so this has to be interior-mutable.
/// Atomics are the cheapest thing that satisfies that without a lock on the
/// per-request path.
#[derive(Default)]
pub struct ByteCounters {
    received: std::sync::atomic::AtomicU64,
    sent: std::sync::atomic::AtomicU64,
}

impl ByteCounters {
    pub fn add_received(&self, n: usize) {
        self.received
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn add_sent(&self, n: usize) {
        self.sent
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn received(&self) -> u64 {
        self.received.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn sent(&self) -> u64 {
        self.sent.load(std::sync::atomic::Ordering::Relaxed)
    }
}
