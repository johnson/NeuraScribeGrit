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
/// The verbs are handed to each network call as one `GritHttpCallbacks` value
/// instead of being installed process-globally at start-up. grit takes its
/// `HttpClient` as a parameter to `http_fetch`/`push_http`, so a global was
/// never required — and it raced: two overlapping operations could have one of
/// them clear the handlers while the other was mid-request, which invoked a
/// release callback on freed closure state. As a struct there is nothing to
/// install, uninstall, or keep in sync, and nothing to get wrong at a call
/// site.
///
/// `GritNetwork.clone`/`fetch`/`push` take one of these and pass it down.
public struct GritHttpBridge {
    /// How long a single request may take before we give up.
    ///
    /// Git over a large repository can legitimately take minutes on a bad
    /// connection, so this is generous on purpose: the alternative is a push
    /// that dies halfway through and leaves the remote in a state the user
    /// has to reason about.
    ///
    /// Global rather than per-instance because the handlers are `@convention(c)`
    /// function pointers, which cannot capture context.
    public nonisolated(unsafe) static var timeout: TimeInterval = 600

    /// Responses larger than this are refused rather than buffered.
    ///
    /// A packfile for a large repository runs to hundreds of megabytes. The
    /// body reaches grit as one contiguous buffer because that is all the
    /// `HttpClient` trait takes, so there is no streaming path to fall back on.
    ///
    /// This is enforced by `Collector` *while* the body arrives, so the cap is a real
    /// bound on peak memory rather than a measurement taken afterwards. The
    /// shim itself holds the body roughly twice — once here, once in the
    /// `Vec` grit takes ownership of — so the process needs headroom for about
    /// twice this figure plus whatever grit needs to index the pack.
    public nonisolated(unsafe) static var maximumResponseBytes = 512 * 1024 * 1024

    public init() {}

    /// The C struct for this call. Plain function pointers, so the struct
    /// carries no lifetime obligation — the buffers it points at are owned by
    /// the `Response` boxes that `handBack` keeps alive.
    public var callbacks: GritHttpCallbacks {
        withUnsafePointer(to: Self.getHandler) { get in
            withUnsafePointer(to: Self.postHandler) { post in
                withUnsafePointer(to: Self.releaseHandler) { release in
                    GritHttpCallbacks(
                        get: UnsafeMutableRawPointer(mutating: get)
                            .assumingMemoryBound(to: GritHttpGetFn.self).pointee,
                        post: UnsafeMutableRawPointer(mutating: post)
                            .assumingMemoryBound(to: GritHttpPostFn.self).pointee,
                        release: UnsafeMutableRawPointer(mutating: release)
                            .assumingMemoryBound(to: GritHttpReleaseFn.self).pointee
                    )
                }
            }
        }
    }

    // MARK: - Ownership

    /// The response a handler produced, kept alive until grit calls `release`.
    ///
    /// The C struct handed to Rust holds raw pointers into `body`, `finalURL`
    /// and `error`, so this object must outlive the call. A +1 reference
    /// travels in the struct's `context` field and `release` consumes it.
    private final class Response {
        /// Held as `Data` rather than `[UInt8]` so the body is not copied a second
        /// time on its way to grit. A packfile runs to hundreds of megabytes,
        /// and every full copy of one is memory this device may not have.
        let body: Data
        /// NUL-terminated, so the pointer we hand out is a valid C string.
        ///
        /// Held as `[CChar]` rather than `CString` because `CString` is no
        /// longer exported by Foundation in Swift 6.4, and because keeping the
        /// bytes owned by the box is what makes the pointer safe to hand out.
        let finalURLBytes: [CChar]
        let errorBytes: [CChar]

        init(body: Data, finalURL: String?, error: String?) {
            self.body = body
            self.finalURLBytes = finalURL.flatMap(Self.nulTerminated) ?? []
            self.errorBytes = error.flatMap(Self.nulTerminated) ?? []
        }

        private static func nulTerminated(_ text: String) -> [CChar]? {
            guard let utf8 = text.data(using: .utf8) else { return nil }
            return utf8.map { CChar(bitPattern: $0) } + [0]
        }
    }

    // MARK: - C callbacks

    // `static let` rather than instance methods: a `@convention(c)` function
    // pointer cannot capture context, so the handlers must be non-capturing and
    // reach their configuration through these statics.
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
        // Balances the `passRetained` in `handBack`. If this ever ran twice for
        // one response it would over-release, so the contract is exactly one
        // release per response, which is what the Rust side guarantees.
        Unmanaged<Response>.fromOpaque(context).release()
    }

    // MARK: - Request

    /// The result of one request, as filled in by the URLSession callback.
    ///
    /// A reference type with a lock, because the callback runs on a session
    /// queue while the caller reads it after the semaphore is signalled. The
    /// semaphore orders the two, but not formally, so the lock makes it correct
    /// rather than merely working.
    /// Collects one response, chunk by chunk, and stops the transfer the moment
    /// it would exceed the cap.
    ///
    /// `URLSession.dataTask(with:completionHandler:)` cannot do this. It hands back a
    /// fully materialised `Data`, so a size check written against it runs *after* the whole
    /// body has already been allocated. On device that is not a near miss: the
    /// process is killed at its memory high-water mark inside CFNetwork's own
    /// accumulation, before any of our code runs again, and the app dies with
    /// no error at all.
    ///
    /// Receiving `didReceive data:` as it arrives is the only place a cap can bound anything. The
    /// overflow check happens *before* the append, so the buffer never exceeds
    /// the cap at all rather than exceeding it and then being measured.
    private final class Collector: NSObject, URLSessionDataDelegate {
        private let lock = NSLock()
        private let limit: Int
        private let semaphore = DispatchSemaphore(value: 0)
        private var bytes = Data()
        private var responseURL: String?
        private var status: Int32 = 0
        private var failure: String?
        /// Set when the cap stopped the transfer, so the cancellation that
        /// follows is not reported as the error instead of the real reason.
        private var overflowed = false

        init(limit: Int) {
            self.limit = limit
        }

        var data: Data? { failure == nil ? bytes : nil }
        var finalURL: String? { responseURL }
        var statusCode: Int32 { status }
        var message: String? { failure }

        /// Block the calling (non-main) thread until the transfer is done.
        func wait() { semaphore.wait() }

        /// What the user is told when a response is refused.
        ///
        /// Says "sync" rather than "fetch" because the same path serves a
        /// push, and the size unit is formatted rather than assumed: the cap is
        /// configurable, so a test-sized cap must not be reported as "0 MiB".
        static func tooLarge(_ limit: Int) -> String {
            let formatter = ByteCountFormatter()
            formatter.countStyle = .binary
            formatter.allowsNonnumericFormatting = false
            let readable = formatter.string(fromByteCount: Int64(limit))
            return "the server sent more than \(readable) in one response; " +
                "this repository is too large to sync on this device"
        }

        func urlSession(
            _ session: URLSession,
            dataTask: URLSessionDataTask,
            didReceive response: URLResponse,
            completionHandler: @escaping (URLSession.ResponseDisposition) -> Void
        ) {
            lock.lock()
            if let http = response as? HTTPURLResponse {
                status = Int32(http.statusCode)
            }
            responseURL = response.url?.absoluteString
            lock.unlock()
            completionHandler(.allow)
        }

        func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
            lock.lock()
            defer { lock.unlock() }
            guard !overflowed else { return }
            guard bytes.count <= limit - data.count else {
                overflowed = true
                failure = Self.tooLarge(limit)
                dataTask.cancel()
                return
            }
            bytes.append(data)
        }

        func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
            lock.lock()
            // A cancel we issued ourselves must not overwrite why we cancelled.
            if failure == nil, let error {
                failure = error.localizedDescription
            }
            lock.unlock()
            semaphore.signal()
        }
    }

    /// Run one request to completion and package it for Rust.
    private static func perform(
        _ request: URLRequest,
        auth: UnsafePointer<CChar>?,
        gitProtocol: UnsafePointer<CChar>?
    ) -> GritHttpResponse {
        var request = request
        request.timeoutInterval = Self.timeout
        if let auth {
            request.setValue(String(cString: auth), forHTTPHeaderField: "Authorization")
        }
        // Protocol v2 is deliberately not requested. grit's push path speaks
        // v0/v1 only; asking for v2 on fetch and not on push is how you end up
        // with a backend that clones fine and cannot push.
        if let gitProtocol {
            request.setValue(String(cString: gitProtocol), forHTTPHeaderField: "Git-Protocol")
        }

        let collector = Collector(limit: Self.maximumResponseBytes)
        let session = URLSession(configuration: .ephemeral, delegate: collector, delegateQueue: nil)
        defer { session.finishTasksAndInvalidate() }

        session.dataTask(with: request).resume()
        collector.wait()

        if let message = collector.message { return failure(message) }
        guard let data = collector.data else {
            return failure(GritBridgeError.noResponse.localizedDescription)
        }
        return handBack(body: data, finalURL: collector.finalURL, error: nil, status: collector.statusCode)
    }

    // MARK: - Handoff

    /// Package a response and hand Rust a +1 pointer to its box.
    ///
    /// `passRetained`, not `passUnretained`. The local `response` below dies
    /// when this function returns, and grit calls `release` much later, so an
    /// unretained pointer would be a use-after-free on every single request.
    /// The +1 is balanced by the `release` handler.
    private static func handBack(body: Data, finalURL: String?, error: String?, status: Int32) -> GritHttpResponse {
        let response = Response(body: body, finalURL: finalURL, error: error)
        let context = Unmanaged.passRetained(response).toOpaque()
        return response.body.withUnsafeBytes { bodyBuffer in
            response.finalURLBytes.withUnsafeBufferPointer { finalURLBuffer in
                response.errorBytes.withUnsafeBufferPointer { errorBuffer in
                    build(
                        status: status,
                        body: bodyBuffer.baseAddress?.assumingMemoryBound(to: UInt8.self),
                        bodyLen: response.body.count,
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
        handBack(body: Data(), finalURL: nil, error: message, status: 0)
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
