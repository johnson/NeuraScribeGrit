//  The HTTP bridge, against a real socket.
//
//  The cap on a response is the difference between "the app tells the user the
//  repository is too large" and "the app is killed by jetsam with no error at
//  all", and that difference lives entirely in *when* the check runs. So these
//  tests drive an actual listener rather than asserting on a helper: a
//  URLSession.dataTask returns a whole materialised Data, and no cap written
//  against one can bound anything.
//
//  The pair is the point. The same server, the same code, one body under the
//  cap and one over it: the first must reach grit, the second must be stopped
//  mid-transfer. Only a check that runs while the body arrives can tell those
//  two apart, and only a rewrite that preserved normal responses can pass both.

import Foundation
import Network
import Testing
@testable import NeuraScribeGrit

/// A one-shot HTTP/1.1 server on an ephemeral loopback port.
private final class OneShotServer {
    private let listener: NWListener

    enum OneShotServerError: Error { case neverBecameReady }

    /// Bytes actually written to the socket, so a test can tell "refused the
    /// rest of the body" from "received all of it and then complained".
    private var written = 0
    private let lock = NSLock()

    init(body: Data, contentType: String) throws {
        let params = NWParameters.tcp
        params.allowLocalEndpointReuse = true
        let listener = try NWListener(using: params, on: .any)
        self.listener = listener
        // The port is not known until the listener is actually ready, and an
        // ephemeral one reports port 0 in the meantime -- which produced a URL
        // of 127.0.0.1:0 and made every test here pass or fail for the wrong
        // reason. Ready is the first state worth trusting.
        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { state in
            if case .ready = state { ready.signal() }
        }

        let header = "HTTP/1.1 200 OK\r\nContent-Type: \(contentType)\r\nContent-Length: \(body.count)\r\nConnection: close\r\n\r\n"
            .data(using: .utf8) ?? Data()

        listener.newConnectionHandler = { [weak self] connection in
            connection.start(queue: .global(qos: .utility))
            connection.receive(minimumIncompleteLength: 1, maximumLength: 65536) { _, _, _, _ in
                let payload = header + body
                connection.send(content: payload, completion: .contentProcessed { error in
                    if error == nil {
                        self?.lock.lock()
                        self?.written += payload.count
                        self?.lock.unlock()
                    }
                    connection.cancel()
                })
            }
        }
        listener.start(queue: .global(qos: .utility))
        guard ready.wait(timeout: .now() + 5) == .success,
              let bound = listener.port?.rawValue, bound != 0 else {
            listener.cancel()
            throw OneShotServerError.neverBecameReady
        }
        precondition(bound != 0)
    }

    /// Read from the listener rather than stored: init cannot know the port
    /// until the listener is ready, but the connection handler is installed
    /// before that and captures self, so a stored port would have to be
    /// initialized before it exists.

    var url: String { "http://127.0.0.1:\(listener.port?.rawValue ?? 0)/repo.git" }

    var bytesWritten: Int {
        lock.lock()
        defer { lock.unlock() }
        return written
    }

    func stop() { listener.cancel() }
}

/// Bytes that look like a git advertisement well enough to reach the packfile
/// stage, so an over-cap response is refused on *size* and not on content.
private func gitLookingBody(_ count: Int) -> Data {
    var data = Data()
    data.append("001e# service=git-upload-pack\n0000".data(using: .utf8) ?? Data())
    while data.count < count { data.append(0x41) }
    return data
}

/// A scratch directory that removes itself, so a failed fetch cannot leave one
/// behind for the next run to trip over.
private func scratch(_ label: String) -> URL {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("nsg-bridge-\(label)-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir
}

/// Runs a fetch that is expected to fail, and returns what the shim said.
/// "" means it unexpectedly succeeded.
private func fetchMessage(against url: String, in directory: URL, capBytes: Int) -> String {
    GritHttpBridge.maximumResponseBytes = capBytes
    defer { GritHttpBridge.maximumResponseBytes = 512 * 1024 * 1024 }

    let destination = directory.appendingPathComponent("clone")
    do {
        _ = try GritNetwork.clone(url: url, to: destination.path)
        return ""
    } catch {
        return error.localizedDescription
    }
}

// Serialized because the cap is process-global, exactly as `timeout` is: the C
// function pointers cannot capture context, so both are statics. Swift Testing
// otherwise runs these three in parallel, and one test restoring the default
// while another has lowered it is a race, not a failure of the bridge.
@Suite("The HTTP bridge bounds a response while it is still arriving", .serialized)
struct GritHttpBridgeTests {
    private let advertisement = "application/x-git-upload-pack-advertisement"

    /// The control: a body under the cap must still reach grit.
    ///
    /// The bridge was rewritten from dataTask to a delegate session, so this
    /// is the test that would catch a rewrite which stops delivering responses
    /// at all. Over-cap must fail; under-cap must not.
    @Test func aResponseUnderTheCapStillReachesGrit() throws {
        let body = gitLookingBody(64 * 1024)
        let server = try OneShotServer(body: body, contentType: advertisement)
        defer { server.stop() }

        let dir = scratch("ok")
        defer { try? FileManager.default.removeItem(at: dir) }

        let message = fetchMessage(against: server.url, in: dir, capBytes: 4 * 1024 * 1024)

        #expect(!message.contains("more than"), "a body under the cap must not be refused on size")
        #expect(!message.contains("too large"), "a body under the cap must not be called too large: \(message)")
    }

    /// An over-cap response is refused, and the refusal is the cap's own
    /// message rather than a transport failure the user cannot act on.
    @Test func aResponseOverTheCapIsRefusedWithAnActionableMessage() throws {
        let body = gitLookingBody(4 * 1024 * 1024)
        let server = try OneShotServer(body: body, contentType: advertisement)
        defer { server.stop() }

        let dir = scratch("big")
        defer { try? FileManager.default.removeItem(at: dir) }

        let message = fetchMessage(against: server.url, in: dir, capBytes: 256 * 1024)

        #expect(message.contains("too large"), "the user needs to be told why: \(message)")
        #expect(!message.contains("cancelled"), "our own cancellation must not be reported as the reason: \(message)")
    }

    /// The transfer is stopped rather than finished and then measured.
    ///
    /// This is the assertion the old code could not have passed: a cap checked
    /// after the body exists has, by definition, already paid for all of it.
    @Test func anOverCapResponseIsNotFullyDownloadedFirst() throws {
        let body = gitLookingBody(8 * 1024 * 1024)
        let server = try OneShotServer(body: body, contentType: advertisement)
        defer { server.stop() }

        let dir = scratch("early")
        defer { try? FileManager.default.removeItem(at: dir) }

        let message = fetchMessage(against: server.url, in: dir, capBytes: 128 * 1024)
        #expect(message.contains("too large"), "precondition: the cap fired (\(message))")

        // Let the socket flush what it already accepted. The point is the order
        // of magnitude, not an exact byte count: 8 MiB against a 128 KiB cap is
        // about 64x, so finishing the transfer could not be mistaken for it.
        Thread.sleep(forTimeInterval: 0.5)
        #expect(
            server.bytesWritten < body.count,
            "the whole \(body.count)-byte body was transferred before the cap was noticed"
        )
    }
}