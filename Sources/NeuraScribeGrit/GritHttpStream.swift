import Foundation

import neurascribe_gritFFI

/// A POST whose body is read on demand instead of being collected first.
///
/// This is the other half of the fix for a pull being killed by the iOS
/// per-process memory limit. GritHttpBridge can only hand grit a finished body,
/// which means the whole packfile is resident in Swift before grit sees a byte
/// of it. Here the body never accumulates: URLSession pushes what arrives into
/// a small buffer, grit pulls from it, and the packfile is unpacked straight
/// into the odb as it streams by.
///
/// The pull and the push meet at an NSCondition. grit calls read from whichever
/// thread its fetch is on; URLSession delivers on its own queue.
final class GritHttpStreamBody: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    private let condition = NSCondition()

    /// How often the reader wakes to re-check for data.
    private static let readPollSeconds: TimeInterval = 5
    /// How long the reader tolerates a quiet body before calling it a stall.
    ///
    /// Long enough to cover a server generating a pack without sending a byte,
    /// short enough that a dead connection surfaces while someone can still see it.
    private static let readStallSeconds: TimeInterval = 300
    /// How far URLSession may run ahead of grit before the task is paused.
    ///
    /// `pending` is the one place a body can still accumulate, and nothing
    /// upstream bounds it on its own: URLSession keeps delivering while grit is
    /// busy, and grit's consumer is far slower than the network (it hashes and
    /// indexes the pack as it goes, and older paths inflated every object into
    /// its own loose file). Without a ceiling a large pack ends up resident here
    /// in full -- the way a pull of a real repository was killed by the iOS
    /// per-process limit even after the body stopped being collected up front.
    /// Suspending the task stops the read at the socket, so the server (and TCP)
    /// throttle the transfer instead of this process's memory.
    private static let highWaterMark = 8 * 1024 * 1024
    /// How far the buffer must drain before the task is allowed to run again.
    private static let lowWaterMark = 2 * 1024 * 1024
    /// Whether the task is suspended by the water marks. The flag keeps suspend
    /// and resume paired, since suspension counts are not idempotent.
    private var suspended = false
    private var pending = Data()
    private var finished = false
    private var failureBytes: [CChar] = []
    private var responseStatus: Int32 = 0
    private var responseSeen = false
    private var failed = false
    private var received: Int = 0
    private var session: URLSession?
    private var task: URLSessionDataTask?

    /// Start the request and return the object the C hooks will drive.
    ///
    /// Returns nil when the request could not be built at all, which the C side
    /// reports as a null handle.
    static func start(
        url: String,
        auth: String?,
        contentType: String,
        accept: String,
        body: UnsafeRawPointer?,
        bodyLen: Int,
        gitProtocol: String?
    ) -> Unmanaged<GritHttpStreamBody>? {
        guard let parsed = URL(string: url) else { return nil }
        var request = URLRequest(url: parsed)
        request.httpMethod = "POST"
        request.setValue(contentType, forHTTPHeaderField: "Content-Type")
        request.setValue(accept, forHTTPHeaderField: "Accept")
        if let gitProtocol { request.setValue(gitProtocol, forHTTPHeaderField: "Git-Protocol") }
        if let auth { request.setValue(auth, forHTTPHeaderField: "Authorization") }
        if let body, bodyLen > 0 {
            request.httpBody = Data(bytes: body, count: bodyLen)
        }

        let stream = GritHttpStreamBody()
        // A nil delegate queue means URLSession makes a serial one for us, which
        // is what lets the delegate append without a second lock on the push side.
        let configuration = URLSessionConfiguration.ephemeral
        // The default is 60s for the whole request, body included. A
        // packfile over a phone link routinely runs past that, and the
        // failure surfaces as URLError.timedOut with nothing in the
        // message about a git problem. The resource timeout is the one
        // that has to cover a large transfer, so it is raised well past
        // any plausible fetch; the connection is not held open idle
        // because waitsForConnectivity only covers the gap before it.
        configuration.timeoutIntervalForRequest = 3600
        configuration.timeoutIntervalForResource = 3600
        configuration.waitsForConnectivity = true
        configuration.allowsExpensiveNetworkAccess = true
        configuration.allowsConstrainedNetworkAccess = true
        let session = URLSession(configuration: configuration, delegate: stream, delegateQueue: nil)
        stream.session = session
        let task = session.dataTask(with: request)
        stream.task = task
        task.resume()
        return Unmanaged.passRetained(stream)
    }

    /// The all-zero stream the C side reads as could-not-start.
    static func notStarted() -> GritHttpStream {
        GritHttpStream(
            handle: nil,
            ops: GritHttpStreamOps(status: nil, read: nil, error: nil, close: nil)
        )
    }
    // MARK: - Pull side, called from grit's thread

    /// The HTTP status, waiting for the response headers if they have not
    /// landed yet. The buffered path only knows the status once the whole
    /// request has completed, so this is deliberately no weaker.
    func waitForStatus() -> Int32 {
        condition.lock()
        defer { condition.unlock() }
        while !responseSeen && !failed {
            condition.wait()
        }
        return responseStatus
    }

    /// Move up to cap bytes out of the buffer, blocking until there are some or
    /// the body has ended. Returns 0 only at the end.
    func read(into buf: UnsafeMutableRawPointer, cap: Int) -> Int {
        condition.lock()
        defer { condition.unlock() }
        // A bounded wait, not an unbounded one. The server sends response
        // headers and can then go quiet for a long time while it builds the
        // pack; waiting forever turns that into a hang that reports nothing,
        // which is worse than an error that says the transfer stalled.
        var waited = 0.0
        while pending.isEmpty && !finished {
            if !condition.wait(until: Date(timeIntervalSinceNow: Self.readPollSeconds)) {
                waited += Self.readPollSeconds
                if waited >= Self.readStallSeconds {
                    failed = true
                    finished = true
                    let stall = "stalled after \(Int(waited))s with \(received) bytes read"
                    failureBytes = Array(stall.utf8.map { CChar(bitPattern: $0) }) + [0]
                    print("[stream] \(stall)")
                    return 0
                }
            }
        }
        let count = min(pending.count, max(cap, 0))
        if count > 0 {
            pending.copyBytes(to: buf.assumingMemoryBound(to: UInt8.self), count: count)
            pending.removeFirst(count)
            received += count
            // Let the network run again once the reader has caught up.
            if suspended, pending.count <= Self.lowWaterMark {
                suspended = false
                task?.resume()
            }
        }
        return count
    }

    /// The failure message, or nil. Valid until close releases the box.
    var errorBytesPointer: UnsafePointer<CChar>? {
        failureBytes.isEmpty ? nil : failureBytes.withUnsafeBufferPointer { $0.baseAddress }
    }

    /// Tear the request down. Safe to call more than once; the shim's drop path
    /// relies on that.
    func close() {
        task?.cancel()
        task = nil
        session?.finishTasksAndInvalidate()
        session = nil
        condition.lock()
        finished = true
        pending.removeAll(keepingCapacity: false)
        condition.broadcast()
        condition.unlock()
    }

    // MARK: - Push side, URLSessionDataDelegate

    func urlSession(
        _ session: URLSession,
        dataTask: URLSessionDataTask,
        didReceive response: URLResponse,
        completionHandler: @escaping (URLSession.ResponseDisposition) -> Void
    ) {
        condition.lock()
        responseStatus = Int32((response as? HTTPURLResponse)?.statusCode ?? 0)
        responseSeen = true
        condition.broadcast()
        condition.unlock()
        completionHandler(.allow)
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        condition.lock()
        pending.append(data)
        // Stop the transfer from outrunning the reader. `suspend` takes effect
        // asynchronously, so a little more may still land; the mark bounds the
        // backlog, it is not a hard limit on a single delivery.
        if !suspended, pending.count >= Self.highWaterMark {
            suspended = true
            dataTask.suspend()
        }
        condition.broadcast()
        condition.unlock()
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        condition.lock()
        if let error {
            failed = true
            failureBytes = Array(error.localizedDescription.utf8.map { CChar(bitPattern: $0) }) + [0]
        }
        finished = true
        condition.broadcast()
        condition.unlock()
    }
}

// MARK: - C hooks
//
// A @convention(c) function pointer cannot capture, so every hook is a
// non-capturing static and recovers its object from the handle the shim hands
// back. That handle is a +1 reference: the shim releases it in close, which is
// the only thing that can free the box.

extension GritHttpBridge {
    static let streamOpenHandler: GritHttpStreamOpenFn = {
        url, auth, contentType, accept, body, bodyLen, gitProtocol in
        guard let url else { return GritHttpStreamBody.notStarted() }
        let started = GritHttpStreamBody.start(
            url: String(cString: url),
            auth: auth.map { String(cString: $0) },
            contentType: contentType.map { String(cString: $0) } ?? "",
            accept: accept.map { String(cString: $0) } ?? "",
            body: body,
            bodyLen: bodyLen,
            gitProtocol: gitProtocol.map { String(cString: $0) }
        )
        guard let started else { return GritHttpStreamBody.notStarted() }

        let handle = started.toOpaque()
        return GritHttpStream(
            handle: handle,
            ops: GritHttpStreamOps(
                status: streamStatusHook,
                read: streamReadHook,
                error: streamErrorHook,
                close: streamCloseHook
            )
        )
    }

    private static let streamStatusHook: @convention(c) (UnsafeMutableRawPointer?) -> Int32 = { handle in
        guard let handle else { return 0 }
        return Unmanaged<GritHttpStreamBody>.fromOpaque(handle).takeUnretainedValue().waitForStatus()
    }

    private static let streamReadHook: @convention(c) (UnsafeMutableRawPointer?, UnsafeMutablePointer<UInt8>?, Int) -> Int = { handle, buf, cap in
        guard let handle, let buf, cap > 0 else { return 0 }
        let count = Unmanaged<GritHttpStreamBody>.fromOpaque(handle)
            .takeUnretainedValue()
            .read(into: UnsafeMutableRawPointer(buf), cap: cap)
        return count
    }

    private static let streamErrorHook: @convention(c) (UnsafeMutableRawPointer?) -> UnsafePointer<CChar>? = { handle in
        guard let handle else { return nil }
        return Unmanaged<GritHttpStreamBody>.fromOpaque(handle)
            .takeUnretainedValue()
            .errorBytesPointer
    }

    private static let streamCloseHook: @convention(c) (UnsafeMutableRawPointer?) -> Void = { handle in
        guard let handle else { return }
        // takeUnretainedValue first, so close() runs while the box is still ours.
        let stream = Unmanaged<GritHttpStreamBody>.fromOpaque(handle).takeUnretainedValue()
        stream.close()
        Unmanaged<GritHttpStreamBody>.fromOpaque(handle).release()
    }
}
