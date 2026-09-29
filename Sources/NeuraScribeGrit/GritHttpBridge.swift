//  GritHttpBridge — the Swift half of grit-lib's `HttpClient` seam.
//
//  grit does not bundle an HTTP stack (`http-ureq` is off by default so that
//  embedders bring their own), and on iOS we should not: URLSession already
//  provides TLS, keychain-backed trust evaluation and ATS policy, all kept
//  current by the system. So the shim carries no TLS dependency at all and
//  calls in here instead.
//
//  grit's `http_fetch` and `push_http` are synchronous, so this bridge blocks
//  the calling thread until the session completes. Git operations already run
//  off the main thread, so that is acceptable — but it is why `install()` must
//  never be reached from a UI path.

import Foundation

#if canImport(neurascribe_gritFFI)
import neurascribe_gritFFI
#endif

/// Supplies HTTP for the grit backend.
///
/// Call `install()` once during app start-up, before the first network
/// operation. Operations that only touch the working tree do not need it.
public enum GritHttpBridge {
    /// How long a single request may take before we give up.
    ///
    /// Git over a large repository can legitimately take minutes on a bad
    /// connection, so this is generous on purpose: the alternative is a push
    /// that dies halfway through and leaves the remote in a state the user
    /// has to reason about.
    public static var timeout: TimeInterval = 600

    /// Responses larger than this are refused rather than buffered.
    ///
    /// A packfile for a large repository runs to hundreds of megabytes. The
    /// body reaches grit as one contiguous buffer because that is all the
    /// `HttpClient` trait takes, so there is no streaming path to fall back on.
    public static var maximumResponseBytes = 512 * 1024 * 1024

    /// Whether the bridge is currently installed.
    public private(set) static var isInstalled = false

    // MARK: - Ownership

    /// The response a handler produced, kept alive until grit calls `release`.
    ///
    /// The C struct handed to Rust holds raw pointers into `body`, `finalURL`
    /// and `error`, so this object must outlive the call. It is kept alive by an
    /// unretained `Unmanaged` pointer travelling in the struct's `context`
    /// field, which `release` converts back.
    private final class Response {
        let body: [UInt8]
        /// NUL-terminated, so the pointer we hand out is a valid C string.
        ///
        /// Held as `[CChar]` rather than `CString` because `CString` is no
        /// longer exported by Foundation in Swift 6.4, and because keeping the
        /// bytes owned by the box is what makes the pointer safe to hand out.
        let finalURLBytes: [CChar]
        let errorBytes: [CChar]

        init(body: [UInt8], finalURL: String?, error: String?) {
            self.body = body
            self.finalURLBytes = finalURL.flatMap(Self.nulTerminated) ?? []
            self.errorBytes = error.flatMap(Self.nulTerminated) ?? []
        }

        private static func nulTerminated(_ text: String) -> [CChar]? {
            guard let utf8 = text.data(using: .utf8) else { return nil }
            return utf8.map { CChar(bitPattern: $0) } + [0]
        }
    }

    // MARK: - Lifecycle

    /// Install the bridge. Safe to call more than once; the first call wins.
    public static func install() {
        guard !isInstalled else { return }
        isInstalled = true
        grit_http_set_handlers(getHandler, postHandler, releaseHandler)
    }

    /// Remove the bridge so the next network call fails with a clear message
    /// instead of appearing to hang.
    public static func uninstall() {
        guard isInstalled else { return }
        isInstalled = false
        grit_http_set_handlers(nil, nil, nil)
    }

    // MARK: - C callbacks

    private static let getHandler: GritHttpGetFn = { url, auth, gitProtocol in
        guard let url, let parsed = URL(string: String(cString: url)) else {
            return failure("the URL is not parseable")
        }
        var request = URLRequest(url: parsed)
        request.httpMethod = "GET"
        return perform(request, auth: auth, gitProtocol: gitProtocol)
    }

    private static let postHandler: GritHttpPostFn = { url, auth, contentType, accept, body, bodyLen, gitProtocol in
        guard let url, let parsed = URL(string: String(cString: url)) else {
            return failure("the URL is not parseable")
        }
        var request = URLRequest(url: parsed)
        request.httpMethod = "POST"
        if let contentType {
            request.setValue(String(cString: contentType), forHTTPHeaderField: "Content-Type")
        }
        if let accept {
            request.setValue(String(cString: accept), forHTTPHeaderField: "Accept")
        }
        if let body, bodyLen > 0 {
            request.httpBody = Data(bytes: body, count: bodyLen)
        }
        return perform(request, auth: auth, gitProtocol: gitProtocol)
    }

    private static let releaseHandler: GritHttpReleaseFn = { response in
        guard let response else { return }
        let context = response.pointee.context
        guard let context else { return }
        // Balances the `passUnretained` in `handBack`. If this ever runs twice
        // for one response it would over-release, so the contract is exactly
        // one release per response, which is what the Rust side guarantees.
        Unmanaged<Response>.fromOpaque(context).release()
    }

    // MARK: - Request

    /// The result of one request, as filled in by the URLSession callback.
    ///
    /// A reference type with a lock, because the callback runs on a session
    /// queue while the caller reads it after the semaphore is signalled. The
    /// semaphore orders the two, but not formally, so the lock makes it correct
    /// rather than merely working.
    private final class Slot {
        private let lock = NSLock()
        private var value: (status: Int32, data: Data?, url: String?, message: String?)?

        func fill(_ new: (status: Int32, data: Data?, url: String?, message: String?)) {
            lock.lock()
            value = new
            lock.unlock()
        }

        var result: (status: Int32, data: Data?, url: String?, message: String?)? {
            lock.lock()
            defer { lock.unlock() }
            return value
        }
    }

    /// Run one request to completion and package it for Rust.
    private static func perform(
        _ request: URLRequest,
        auth: UnsafePointer<CChar>?,
        gitProtocol: UnsafePointer<CChar>?
    ) -> GritHttpResponse {
        var request = request
        request.timeoutInterval = timeout
        if let auth {
            request.setValue(String(cString: auth), forHTTPHeaderField: "Authorization")
        }
        // Protocol v2 is deliberately not requested. grit's push path speaks
        // v0/v1 only; asking for v2 on fetch and not on push is how you end up
        // with a backend that clones fine and cannot push.
        if let gitProtocol {
            request.setValue(String(cString: gitProtocol), forHTTPHeaderField: "Git-Protocol")
        }

        let slot = Slot()
        let semaphore = DispatchSemaphore(value: 0)
        URLSession.shared.dataTask(with: request) { data, response, error in
            if let error {
                slot.fill((0, nil, nil, error.localizedDescription))
            } else if let http = response as? HTTPURLResponse {
                slot.fill((Int32(http.statusCode), data, response?.url?.absoluteString, nil))
            } else {
                slot.fill((0, nil, nil, GritBridgeError.noResponse.localizedDescription))
            }
            semaphore.signal()
        }
        .resume()
        semaphore.wait()

        guard let (status, data, url, message) = slot.result else {
            return failure(GritBridgeError.noResponse.localizedDescription)
        }
        if let message { return failure(message) }
        guard let data else { return failure(GritBridgeError.noResponse.localizedDescription) }
        guard data.count <= maximumResponseBytes else {
            return failure("the server sent \(data.count) bytes, over the \(maximumResponseBytes)-byte limit")
        }
        return handBack(body: [UInt8](data), finalURL: url, error: nil, status: status)
    }

    // MARK: - Handoff

    /// Package a response and hand Rust an unretained pointer to its box.
    private static func handBack(body: [UInt8], finalURL: String?, error: String?, status: Int32) -> GritHttpResponse {
        let response = Response(body: body, finalURL: finalURL, error: error)
        let context = Unmanaged.passUnretained(response).toOpaque()
        return response.body.withUnsafeBytes { bodyBuffer in
            response.finalURLBytes.withUnsafeBufferPointer { finalURLBuffer in
                response.errorBytes.withUnsafeBufferPointer { errorBuffer in
                    build(
                        status: status,
                        body: bodyBuffer.baseAddress?.assumingMemoryBound(to: UInt8.self),
                        bodyLen: body.count,
                        finalURL: finalURLBuffer.isEmpty ? nil : finalURLBuffer.baseAddress,
                        error: errorBuffer.isEmpty ? nil : errorBuffer.baseAddress,
                        context: context
                    )
                }
            }
        }
    }

    /// Assemble the struct, substituting NULL for any absent buffer.
    ///
    /// A `nil` `[CChar]` has no `withUnsafeBufferPointer` to nest into, so the
    /// optionals are flattened here instead.
    private static func build(
        status: Int32,
        body: UnsafePointer<UInt8>?,
        bodyLen: Int,
        finalURL: UnsafePointer<CChar>?,
        error: UnsafePointer<CChar>?,
        context: UnsafeMutableRawPointer?
    ) -> GritHttpResponse {
        GritHttpResponse(
            status: status,
            body: body,
            body_len: bodyLen,
            final_url: finalURL,
            error: error,
            context: context
        )
    }

    private static func failure(_ message: String) -> GritHttpResponse {
        handBack(body: [], finalURL: nil, error: message, status: 0)
    }
}

private enum GritBridgeError: LocalizedError {
    case noResponse

    var errorDescription: String? {
        switch self {
        case .noResponse: "the HTTP session produced no response"
        }
    }
}
