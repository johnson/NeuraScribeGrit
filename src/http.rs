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
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use grit_lib::credentials::{Credential, CredentialProvider};
use grit_lib::error::{Error, Result};
use grit_lib::transport::http::HttpClient;

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

static GET: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static POST: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static RELEASE: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// Install Swift's HTTP bridge. Passing a null function pointer clears it,
/// which makes the next network call fail loudly instead of silently doing
/// nothing -- a missing handler must never look like a clean fetch.
#[no_mangle]
pub unsafe extern "C" fn grit_http_set_handlers(
    get: Option<RawGetFn>,
    post: Option<RawPostFn>,
    release: Option<RawReleaseFn>,
) {
    GET.store(get.map_or(std::ptr::null_mut(), |f| f as *mut ()), Ordering::Release);
    POST.store(post.map_or(std::ptr::null_mut(), |f| f as *mut ()), Ordering::Release);
    RELEASE.store(release.map_or(std::ptr::null_mut(), |f| f as *mut ()), Ordering::Release);
}

/// A response after its buffers have been copied out and handed back.
struct Copied {
    status: i32,
    body: Vec<u8>,
    final_url: Option<String>,
}

fn load_get() -> Option<RawGetFn> {
    let p = GET.load(Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        // SAFETY: a non-null pointer here was stored from a `RawGetFn`.
        Some(unsafe { std::mem::transmute(p) })
    }
}

fn load_post() -> Option<RawPostFn> {
    let p = POST.load(Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        // SAFETY: a non-null pointer here was stored from a `RawPostFn`.
        Some(unsafe { std::mem::transmute(p) })
    }
}

/// Copy a response out, then hand it straight back to Swift to free.
///
/// `release` runs on every path, including the error path -- leaking Swift's
/// buffers here would be a slow leak in a long-running app.
fn copy_out(response: RawHttpResponse) -> std::result::Result<Copied, String> {
    let RawHttpResponse { status, body, body_len, final_url, error, context } = response;
    let release: RawReleaseFn = unsafe { std::mem::transmute(RELEASE.load(Ordering::Acquire)) };
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
    credentials: Arc<dyn CredentialProvider + Send + Sync>,
    /// `Authorization` header cached from the last successful exchange, so the
    /// common case is one round trip rather than two.
    auth_header: Mutex<Option<String>>,
}

impl CallbackHttpClient {
    /// Wrap a credential provider. The trait's `fill` is what answers a 401.
    pub fn new(credentials: Arc<dyn CredentialProvider + Send + Sync>) -> Self {
        Self { credentials, auth_header: Mutex::new(None) }
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

impl HttpClient for CallbackHttpClient {
    fn get(&self, url: &str, git_protocol: Option<&str>) -> Result<Vec<u8>> {
        self.get_with_final_url(url, git_protocol).map(|(body, _)| body)
    }

    fn get_with_final_url(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        let get = load_get().ok_or_else(|| {
            Error::Message("no HTTP handler installed; call grit_http_set_handlers first".into())
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
            let copied = copy_out(response)?;
            let final_url = copied.final_url.clone();
            let body = copied.body.clone();
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
        let post = load_post().ok_or_else(|| {
            Error::Message("no HTTP handler installed; call grit_http_set_handlers first".into())
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
            let copied = copy_out(response)?;
            let bytes = copied.body.clone();
            Ok((copied, bytes))
        })
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
#[cfg(feature = "http-ureq")]
pub fn client_for(cred: Option<&CredentialSpec>) -> Result<Arc<dyn HttpClient>> {
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
pub fn client_for(cred: Option<&CredentialSpec>) -> Result<Arc<dyn HttpClient>> {
    Ok(Arc::new(CallbackHttpClient::new(provider_for(cred))))
}
