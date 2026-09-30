//! HTTP plumbing for the network operations.
//!
//! grit-lib implements the whole smart-HTTP side of clone, fetch AND push
//! (`http_fetch`, `push_http`) and asks the embedder for exactly one thing:
//! the two HTTP verbs. That is deliberate -- grit does not force a TLS stack on
//! us, and on iOS we do not want one, because URLSession already provides TLS,
//! keychain-backed trust evaluation and ATS policy, all kept current by the
//! system. So the shim carries no TLS dependency at all: no rustls, no ring, no
//! aws-lc-rs in the iOS binary. It calls back into Swift and the app answers.
//!
//! On the host (the differential suite) there is no Swift, so the `http-ureq`
//! feature wires grit's own ureq-backed client instead. Both satisfy the same
//! trait, so the operations are written once.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::{Arc, Mutex};

use grit_lib::credentials::{Credential, CredentialProvider};
use grit_lib::error::{Error, Result};
use grit_lib::transport::http::{HttpBody, HttpClient};

use crate::types::Credential as CredentialSpec;

/// A credential the app already has in hand (Keychain, or a login form).
///
/// grit would normally shell out to a `credential.helper`; that is a
/// subprocess and therefore unavailable on iOS. The app supplies the password
/// directly instead, which is also what the existing gix backend does.
pub struct StaticCredentialProvider {
    username: Option<String>,
    password: Option<String>,
}

impl StaticCredentialProvider {
    /// Build a provider from the credential the caller passed in. Either half
    /// may be absent, in which case grit fills what it can.
    pub fn new(username: Option<String>, password: Option<String>) -> Self {
        Self { username, password }
    }
}

impl CredentialProvider for StaticCredentialProvider {
    fn fill(&self, input: &Credential) -> Result<Credential> {
        let mut filled = input.clone();
        if filled.username.is_none() {
            filled.username = self.username.clone();
        }
        if filled.password.is_none() {
            filled.password = self.password.clone();
        }
        Ok(filled)
    }

    fn approve(&self, _cred: &Credential) -> Result<()> {
        // Nothing to persist: the app owns the Keychain entry, not a helper.
        Ok(())
    }

    fn reject(&self, _cred: &Credential) -> Result<()> {
        Ok(())
    }
}

// MARK: - Swift callback bridge (the iOS path)

/// One HTTP response handed back from Swift.
///
/// Every pointer must stay valid until `release` is called on the response;
/// Swift owns the buffers and frees them there. The status is passed through
/// rather than folded into an error because the auth dance below needs to see
/// a 401 and retry.
#[repr(C)]
pub struct RawHttpResponse {
    /// HTTP status, or 0 when the request never completed.
    pub status: i32,
    /// Response body. May be NULL when `body_len` is 0.
    pub body: *const u8,
    pub body_len: usize,
    /// URL the request finally resolved to, or NULL when unknown.
    pub final_url: *const c_char,
    /// Failure description, or NULL on success.
    pub error: *const c_char,
    /// Opaque handle to whatever the embedder allocated, returned verbatim to
    /// `release`. Swift needs this because it cannot recover its own object
    /// from the body pointer without assuming something untrue about it.
    pub context: *mut c_void,
}

/// `auth` carries a ready-made `Authorization` header value, or NULL. The
/// shim owns the 401-retry dance (see `with_auth_retry`), so the header is
/// built here rather than in Swift; Swift only has to attach it.
type RawGetFn = extern "C" fn(
    url: *const c_char,
    auth: *const c_char,
    git_protocol: *const c_char,
) -> RawHttpResponse;
type RawPostFn = extern "C" fn(
    url: *const c_char,
    auth: *const c_char,
    content_type: *const c_char,
    accept: *const c_char,
    body: *const u8,
    body_len: usize,
    git_protocol: *const c_char,
) -> RawHttpResponse;
type RawReleaseFn = extern "C" fn(response: *mut RawHttpResponse);

/// How grit pulls a response body out of a handle Swift owns.
///
/// Mirrors GritHttpStreamOps in the header. The handle is the context: a C
/// function pointer cannot capture, so every hook takes the handle back as
/// its first argument.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawStreamOps {
    pub status: Option<extern "C" fn(handle: *mut c_void) -> i32>,
    pub read: Option<extern "C" fn(handle: *mut c_void, buf: *mut u8, cap: usize) -> usize>,
    pub error: Option<extern "C" fn(handle: *mut c_void) -> *const c_char>,
    pub close: Option<extern "C" fn(handle: *mut c_void)>,
}

/// A started request: the body reader, and the object the hooks act on.
///
/// The hooks travel from the embedder to the shim, not the other way round.
/// A C function pointer cannot capture, so the hooks are non-capturing and
/// recover their object from the handle the shim hands back.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawStream {
    pub handle: *mut c_void,
    pub ops: RawStreamOps,
}

/// Starts a request and hands back the reader for its body. An all-zero
/// result means it could not be started.
type RawStreamOpenFn = extern "C" fn(
    url: *const c_char,
    auth: *const c_char,
    content_type: *const c_char,
    accept: *const c_char,
    body: *const u8,
    body_len: usize,
    git_protocol: *const c_char,
) -> RawStream;

/// The three Swift entry points, passed **per call**.
///
/// They used to live in process-global atomics installed by
/// `grit_http_set_handlers`. That was a mistake with teeth: grit takes its
/// `HttpClient` as a parameter to `http_fetch`/`push_http`, so nothing in grit
/// ever wanted a global -- it was invented here, and it raced. Two overlapping
/// network operations could have one of them clear the handlers while the other
/// was mid-request, and because the three pointers were three *separate*
/// atomics a reader could also observe a half-installed set. The result was a
/// release callback invoked on freed Swift closure state, i.e. EXC_BAD_ACCESS
/// inside `copy_out`.
///
/// Handing the triple in as one `#[repr(C)]` struct per call makes the whole set
/// move atomically and removes the install/uninstall dance entirely.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HttpCallbacks {
    /// May be NULL; the operation then fails with a clear message rather than
    /// appearing to succeed having transferred nothing.
    pub get: Option<RawGetFn>,
    /// May be NULL, same rule.
    pub post: Option<RawPostFn>,
    /// May be NULL only if `get` and `post` are too. `copy_out` requires it,
    /// because it is what frees the buffers Swift handed over.
    /// The streaming form of post. NULL means the caller does not stream, and
    /// post_streaming falls back to buffering through post -- which is exactly
    /// grit's own default, so nothing has to be kept in step.
    pub stream_post: Option<RawStreamOpenFn>,
    pub release: Option<RawReleaseFn>,
}

impl HttpCallbacks {
    /// The set to use when the host build wires grit's own ureq client and the
    /// embedder has no Swift (the differential suite).
    pub fn none() -> Self {
        Self { get: None, post: None, stream_post: None, release: None }
    }
}

/// A response after its buffers have been copied out and handed back.
struct Copied {
    status: i32,
    body: Vec<u8>,
    final_url: Option<String>,
}

/// Copy a response out, then hand it straight back to Swift to free.
///
/// `release` runs on every path, including the error path -- leaking Swift's
/// buffers here would be a slow leak in a long-running app.
fn copy_out(
    response: RawHttpResponse,
    release: Option<RawReleaseFn>,
) -> std::result::Result<Copied, String> {
    let RawHttpResponse { status, body, body_len, final_url, error, context } = response;
    let release = release.ok_or_else(|| {
        "the HTTP bridge supplied no release callback, so its buffers would leak".to_string()
    })?;
    let mut out = Copied { status, body: Vec::new(), final_url: None };
    let mut failure = None;

    // SAFETY: the pointers came from the Swift handler and stay valid until we
    // call release, which we call exactly once on both branches.
    unsafe {
        if !error.is_null() {
            failure = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
        } else {
            if !body.is_null() && body_len > 0 {
                out.body.extend_from_slice(std::slice::from_raw_parts(body, body_len));
            }
            if !final_url.is_null() {
                out.final_url = Some(CStr::from_ptr(final_url).to_string_lossy().into_owned());
            }
        }
        release(&mut RawHttpResponse { status, body, body_len, final_url, error, context });
    }

    match failure {
        Some(message) => Err(message),
        None => Ok(out),
    }
}

fn protocol_ptr(value: Option<&str>) -> std::result::Result<Option<CString>, String> {
    match value {
        Some(v) => CString::new(v)
            .map(Some)
            .map_err(|_| "Git-Protocol header contained a NUL".to_string()),
        None => Ok(None),
    }
}

/// `HttpClient` that forwards every request to Swift's URLSession.
///
/// `http_fetch` and `push_http` are synchronous, so the Swift side must block
/// until the session completes. That is fine because git operations already run
/// off the main thread, but it is why this must never sit on a UI path.
pub struct CallbackHttpClient {
    /// Copied in at construction. Reading the callbacks off a value already in
    /// hand is what makes install/uninstall -- and the race it caused --
    /// unnecessary.
    callbacks: HttpCallbacks,
    credentials: Arc<dyn CredentialProvider + Send + Sync>,
    /// `Authorization` header cached from the last successful exchange, so the
    /// common case is one round trip rather than two.
    auth_header: Mutex<Option<String>>,
    /// Bytes that crossed the boundary, for the progress reporter. The
    /// trait methods take `&self`, so this cannot be a plain integer.
    counters: Arc<crate::progress::ByteCounters>,
}

impl CallbackHttpClient {
    /// Wrap a credential provider. The trait's `fill` is what answers a 401.
    pub fn new(
        callbacks: HttpCallbacks,
        credentials: Arc<dyn CredentialProvider + Send + Sync>,
        counters: Arc<crate::progress::ByteCounters>,
    ) -> Self {
        Self { callbacks, credentials, auth_header: Mutex::new(None), counters }
    }

    /// Do one request, retrying exactly once after a 401, the way git does.
    fn with_auth_retry<T, F>(&self, run: F) -> Result<T>
    where
        F: Fn(Option<&str>) -> std::result::Result<(Copied, T), String>,
    {
        let cached = self.auth_header.lock().ok().and_then(|slot| slot.clone());
        let first = run(cached.as_deref()).map_err(Error::Message)?;
        let (response, value) = first;

        if response.status != 401 {
            return finish(response, value);
        }

        // A second challenge means our password is wrong; say so precisely so
        // the app can prompt for a new one instead of retrying forever.
        let input = Credential {
            protocol: Some("https".into()),
            ..Default::default()
        };
        let cred = match self.credentials.fill(&input) {
            Ok(c) if c.username.is_some() => c,
            _ => {
                return Err(Error::Auth(
                    "no credential is available for this remote".to_string(),
                ))
            }
        };
        let header = match basic_auth_header(&cred) {
            Some(h) => h,
            None => {
                return Err(Error::Auth(
                    "server asked for credentials but none were supplied".to_string(),
                ))
            }
        };
        let (response, value) = run(Some(&header)).map_err(Error::Message)?;
        if response.status == 401 {
            let _ = self.credentials.reject(&cred);
            if let Ok(mut slot) = self.auth_header.lock() {
                *slot = None;
            }
            return Err(Error::Auth("the supplied credentials were rejected".to_string()));
        }
        let _ = self.credentials.approve(&cred);
        if let Ok(mut slot) = self.auth_header.lock() {
            *slot = Some(header);
        }
        finish(response, value)
    }
}

fn finish<T>(response: Copied, value: T) -> Result<T> {
    if (200..300).contains(&response.status) {
        Ok(value)
    } else {
        Err(Error::Message(format!("HTTP {} from the git server", response.status)))
    }
}

/// `Basic base64(user:pass)`. An empty password is allowed; a missing username
/// is not, because Basic cannot express it.
fn basic_auth_header(cred: &Credential) -> Option<String> {
    let user = cred.username.as_deref().filter(|s| !s.is_empty())?;
    let pass = cred.password.as_deref().unwrap_or("");
    Some(format!("Basic {}", base64_encode(format!("{user}:{pass}").as_bytes())))
}

/// Minimal standard-alphabet base64, so the shim does not pull an encoder in
/// just to format one header.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// A response body being pulled from Swift chunk by chunk.
///
/// This is the whole point of the streaming path: grit's reader asks for the
/// next piece and Swift hands over only what has arrived, so neither side ever
/// holds the packfile. The status and the 401 dance are settled before the
/// body is handed to grit, because grit will not look at either once it has
/// started consuming.
pub struct StreamBody {
    stream: RawStream,
    /// Set once the body has been closed, so the drop path is idempotent.
    closed: bool,
    /// Diagnostics: how much came off the wire before the body ended,
    /// and in how many reads. An early end is the difference between a
    /// server that closed and a reader that gave up, and the error alone
    /// does not say which.
    chunks: u64,
    bytes: u64,
}

impl StreamBody {
    /// The HTTP status, waiting for the response headers if they have not
    /// arrived yet. 0 when the request never completed.
    fn status(&self) -> i32 {
        match self.stream.ops.status {
            Some(f) => f(self.stream.handle),
            None => 0,
        }
    }

    /// Reject anything that is not a 2xx, reading the small error body so
    /// the message says what the server said. The body is drained here and
    /// nowhere else, so the good path never pays for it.
    fn accepted(mut self, status: i32) -> Result<Self> {
        if (200..300).contains(&status) {
            return Ok(self);
        }
        let detail = self.drain();
        self.close();
        Err(Error::Message(if detail.is_empty() {
            format!("HTTP {status} from the git server")
        } else {
            format!("HTTP {status} from the git server: {detail}")
        }))
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(close) = self.stream.ops.close {
            close(self.stream.handle);
        }
    }

    /// Read whatever Swift has, up to cap. Returns 0 only at the end of the
    /// body; the handle is allowed to block until something arrives.
    fn pull(&mut self, cap: usize) -> Result<Vec<u8>> {
        let read = self
            .stream
            .ops
            .read
            .ok_or_else(|| {
                Error::Message("the HTTP stream bridge supplied no read callback".into())
            })?;
        let mut buf = vec![0u8; cap];
        let n = read(self.stream.handle, buf.as_mut_ptr(), cap);
        buf.truncate(n);
        Ok(buf)
    }

    /// Drain the rest of the body into a string. Only used to turn a non-2xx
    /// response into something worth showing; a 2xx body is never read this way.
    fn drain(&mut self) -> String {
        let mut all = Vec::new();
        loop {
            match self.pull(8 * 1024) {
                Ok(chunk) if !chunk.is_empty() => all.extend_from_slice(&chunk),
                _ => break,
            }
            if all.len() > 64 * 1024 {
                break;
            }
        }
        String::from_utf8_lossy(&all).trim().to_string()
    }

    /// What went wrong, if anything. Read before close: close frees whatever
    /// the string was anchored to.
    fn failure(&self) -> Option<String> {
        let error = self.stream.ops.error?;
        let raw = error(self.stream.handle);
        if raw.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned())
        }
    }
}

// SAFETY: the handle is only ever touched between open and close, both of
// which happen on the thread that made the call; the raw pointer is not
// shared across threads, only moved.
unsafe impl Send for StreamBody {}


impl HttpBody for StreamBody {
    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.closed {
            return Ok(None);
        }
        self.chunks += 1;
        let chunk = self.pull(64 * 1024)?;
        self.bytes += chunk.len() as u64;
        if chunk.is_empty() {
            if let Some(failure) = self.failure() {
                return Err(Error::Message(failure));
            }
            eprintln!(
                "[shim-stream] eof after {} chunks / {} bytes",
                self.chunks, self.bytes
            );
            return Ok(None);
        }
        Ok(Some(chunk))
    }
}
impl HttpClient for CallbackHttpClient {
    fn get(&self, url: &str, git_protocol: Option<&str>) -> Result<Vec<u8>> {
        self.get_with_final_url(url, git_protocol).map(|(body, _)| body)
    }

    fn get_with_final_url(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        let get = self.callbacks.get.ok_or_else(|| {
            Error::Message("no HTTP GET handler was supplied for this operation".into())
        })?;
        let url_c = CString::new(url).map_err(|_| Error::Message("url contained a NUL".into()))?;
        let protocol = protocol_ptr(git_protocol).map_err(Error::Message)?;
        let protocol_ptr = protocol.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        self.with_auth_retry(|auth| {
            let auth_c = auth
                .map(CString::new)
                .transpose()
                .map_err(|e| e.to_string())?;
            let auth_ptr = auth_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
            let response = get(url_c.as_ptr(), auth_ptr, protocol_ptr);
            let copied = copy_out(response, self.callbacks.release)?;
            let final_url = copied.final_url.clone();
            let body = copied.body.clone();
            self.counters.add_received(body.len());
            Ok((copied, (body, final_url)))
        })
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Vec<u8>> {
        let post = self.callbacks.post.ok_or_else(|| {
            Error::Message("no HTTP POST handler was supplied for this operation".into())
        })?;
        let url_c = CString::new(url).map_err(|_| Error::Message("url contained a NUL".into()))?;
        let ct = CString::new(content_type).unwrap_or_default();
        let ac = CString::new(accept).unwrap_or_default();
        let protocol = protocol_ptr(git_protocol).map_err(Error::Message)?;
        let protocol_ptr = protocol.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        self.with_auth_retry(|auth| {
            let auth_c = auth
                .map(CString::new)
                .transpose()
                .map_err(|e| e.to_string())?;
            let auth_ptr = auth_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
            let response = post(
                url_c.as_ptr(),
                auth_ptr,
                ct.as_ptr(),
                ac.as_ptr(),
                body.as_ptr(),
                body.len(),
                protocol_ptr,
            );
            self.counters.add_sent(body.len());
            let copied = copy_out(response, self.callbacks.release)?;
            let bytes = copied.body.clone();
            self.counters.add_received(bytes.len());
            Ok((copied, bytes))
        })
    }
    fn post_streaming(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Box<dyn HttpBody>> {
        let open = match self.callbacks.stream_post {
            Some(open) => open,
            // No streaming handler: the inherited default buffers through post,
            // which is what a caller with no use for streaming gets.
            None => {
                return HttpClient::post_streaming(
                    self,
                    url,
                    content_type,
                    accept,
                    body,
                    git_protocol,
                )
            }
        };
        let url_c = CString::new(url).map_err(|_| Error::Message("url contained a NUL".into()))?;
        let ct = CString::new(content_type).unwrap_or_default();
        let ac = CString::new(accept).unwrap_or_default();
        let protocol = protocol_ptr(git_protocol).map_err(Error::Message)?;
        let protocol_ptr = protocol.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        self.counters.add_sent(body.len());

        let cached = self.auth_header.lock().ok().and_then(|slot| slot.clone());
        let attempt = |auth: Option<&str>| -> Result<StreamBody> {
            let auth_c = auth.map(CString::new).transpose().map_err(|e| Error::Message(e.to_string()))?;
            let auth_ptr = auth_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
            let stream = open(
                url_c.as_ptr(),
                auth_ptr,
                ct.as_ptr(),
                ac.as_ptr(),
                body.as_ptr(),
                body.len(),
                protocol_ptr,
            );
            if stream.handle.is_null() || stream.ops.read.is_none() || stream.ops.close.is_none() {
                return Err(Error::Message(
                    "the HTTP bridge could not start a streaming request".into(),
                ));
            }
            Ok(StreamBody { stream, closed: false, chunks: 0, bytes: 0 })
        };

        let mut stream = attempt(cached.as_deref())?;
        let status = stream.status();
        if status != 401 {
            return Ok(Box::new(stream.accepted(status)?));
        }

        // A challenge means the body is an error page, not a pack. Drop it and
        // go round once with a fresh credential, exactly as the buffered path
        // does; grit never sees either body.
        stream.close();
        let (cred, header) = self.fresh_credential()?;
        let stream = attempt(Some(&header))?;
        let status = stream.status();
        if status == 401 {
            let _ = self.credentials.reject(&cred);
            if let Ok(mut slot) = self.auth_header.lock() {
                *slot = None;
            }
            return Err(Error::Auth("the supplied credentials were rejected".into()));
        }
        let _ = self.credentials.approve(&cred);
        if let Ok(mut slot) = self.auth_header.lock() {
            *slot = Some(header);
        }
        Ok(Box::new(stream.accepted(status)?))
    }
}


impl CallbackHttpClient {
    /// Ask the provider for a fresh credential after a challenge and build the
    /// header from it. Shared with the buffered path so both fail the same way
    /// when there is nothing to offer.
    fn fresh_credential(&self) -> Result<(Credential, String)> {
        let input = Credential {
            protocol: Some("https".into()),
            ..Default::default()
        };
        let cred = match self.credentials.fill(&input) {
            Ok(c) if c.username.is_some() => c,
            _ => {
                return Err(Error::Auth(
                    "no credential is available for this remote".to_string(),
                ))
            }
        };
        match basic_auth_header(&cred) {
            Some(h) => Ok((cred, h)),
            None => Err(Error::Auth(
                "server asked for credentials but none were supplied".to_string(),
            )),
        }
    }
}

/// Build the credential provider for a caller-supplied credential.
pub fn provider_for(cred: Option<&CredentialSpec>) -> Arc<dyn CredentialProvider + Send + Sync> {
    Arc::new(StaticCredentialProvider::new(
        cred
            .map(|c| (!c.username.is_empty()).then(|| c.username.clone()))
            .flatten(),
        cred
            .map(|c| (!c.password.is_empty()).then(|| c.password.clone()))
            .flatten(),
    ))
}

/// The client the network operations run against, via grit's ureq backend.
///
/// The callbacks and the counters are accepted and ignored here. Under this
/// feature grit owns the whole transport, so there is no seam to count bytes at
/// and no Swift side to call back into -- which is why the counters stay at 0
/// and the progress UI shows a phase without a byte total. That is the honest
/// result for this build, not a silent no-op.
#[cfg(feature = "http-ureq")]
pub fn client_for(
    cred: Option<&CredentialSpec>,
    _callbacks: HttpCallbacks,
    _counters: Arc<crate::progress::ByteCounters>,
) -> Result<Arc<dyn HttpClient>> {
    use grit_lib::transport::http::ureq_client::UreqHttpClient;

    let client = match cred {
        Some(c) => UreqHttpClient::new().with_credential_provider(
            Box::new(StaticCredentialProvider::new(
                (!c.username.is_empty()).then(|| c.username.clone()),
                (!c.password.is_empty()).then(|| c.password.clone()),
            )),
        ),
        None => UreqHttpClient::new(),
    };
    Ok(Arc::new(client))
}

/// The client the network operations run against, through Swift URLSession.
#[cfg(not(feature = "http-ureq"))]
pub fn client_for(
    cred: Option<&CredentialSpec>,
    callbacks: HttpCallbacks,
    counters: Arc<crate::progress::ByteCounters>,
) -> Result<Arc<dyn HttpClient>> {
    Ok(Arc::new(CallbackHttpClient::new(callbacks, provider_for(cred), counters)))
}
