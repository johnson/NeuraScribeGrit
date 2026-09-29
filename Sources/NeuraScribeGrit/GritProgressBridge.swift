//  GritProgressBridge — the Swift half of grit's progress seam.
//
//  grit already models progress: http_fetch and push_http take a
//  &mut dyn Progress, and the shim was handing them NoProgress, so every
//  update the remote sent went nowhere.
//
//  The seven hooks are not all equally well informed, and this file is where
//  that gets decided. What grit reports live is reported live; what it only
//  hands back at the end is reported at the end; what it does not expose at all
//  is left out rather than invented. A progress bar showing an invented
//  percentage is worse than no bar.

import Foundation

#if canImport(neurascribe_gritFFI)
import neurascribe_gritFFI
#endif

/// One progress event, in a form the app can render without knowing about git.
///
/// Deliberately a value type holding only fields that mean something. The
/// absence of a number is nil, not 0 — the whole point is that a caller can
/// tell "nothing arrived" apart from "grit did not say".
public enum GritProgressEvent: Sendable, Equatable {
    /// A line from the remote, on grit's own sideband channel.
    ///
    /// This is git's "remote: Counting objects" text, and it is the only
    /// genuinely live signal there is.
    case message(String)

    /// A ref moved locally. Either oid is nil when the ref was created or
    /// deleted.
    case refUpdated(name: String, old: String?, new: String?)

    /// Objects finished arriving.
    ///
    /// 'bytes' is nil when there was no measurable boundary: a local remote has
    /// none, and under the host build grit owns the transport so there is no
    /// seam to count at.
    case received(objects: Int, bytes: UInt64?)

    /// A pack-building phase — building before the pack goes out, uploading
    /// after.
    case packPhase(building: Bool)

    /// The upload finished, having sent 'bytes'.
    case pushed(bytes: UInt64)

    /// The server's verdict on one ref. 'reason' is nil when accepted.
    case pushResult(ref: String, reason: String?)

    /// The update about to be sent, with its local tip.
    case pushNegotiated(ref: String, oid: String)
}

/// Receives progress from the grit backend.
///
/// A closure rather than seven raw function pointers, because nothing here is
/// handed to C by the caller: 'withRegistration' builds the C struct from these
/// closures and tears it down afterwards.
public struct GritProgressBridge: Sendable {
    /// Called for every event, in the order the shim reported it.
    public var onEvent: @Sendable (GritProgressEvent) -> Void

    public init(onEvent: @escaping @Sendable (GritProgressEvent) -> Void) {
        self.onEvent = onEvent
    }

    /// Discards everything. The default, so a caller that does not want progress
    /// does not have to say so at every call site.
    public static let silent = GritProgressBridge { _ in }

    /// Run 'body' with the C struct for this bridge, releasing it afterwards.
    ///
    /// Scoped rather than handing out a bare struct on purpose. The struct holds
    /// a retained context that the C side has no way to release, so a bare
    /// "build it yourself" API would mean every call site had to remember a
    /// matching release — a missed one leaks the sink, an extra one crashes, and
    /// neither should be reachable by forgetting a line.
    public static func withRegistration<R>(
        _ bridge: GritProgressBridge,
        _ body: (GritProgressCallbacks) throws -> R
    ) rethrows -> R {
        let context = Unmanaged.passRetained(Sink(bridge.onEvent)).toOpaque()
        defer { Unmanaged<Sink>.fromOpaque(context).release() }
        return try body(callbacks(for: context))
    }

    /// The C struct. Plain function pointers with no lifetime obligation, which
    /// is what lets it be passed to the shim by value, like GritHttpCallbacks.
    ///
    /// Assigned a field at a time rather than nested: seven levels of
    /// withUnsafePointer defeats the type checker and the compiler gives up with
    /// "failed to produce diagnostic" rather than pointing at the real problem.
    private static func callbacks(for context: UnsafeMutableRawPointer) -> GritProgressCallbacks {
        var result = GritProgressCallbacks()
        withUnsafePointer(to: sidebandHandler) { result.sideband = $0.pointee }
        withUnsafePointer(to: transferHandler) { result.transfer = $0.pointee }
        withUnsafePointer(to: updateTipHandler) { result.update_tip = $0.pointee }
        withUnsafePointer(to: packHandler) { result.pack = $0.pointee }
        withUnsafePointer(to: pushTransferHandler) { result.push_transfer = $0.pointee }
        withUnsafePointer(to: pushUpdateRefHandler) { result.push_update_ref = $0.pointee }
        withUnsafePointer(to: pushNegotiationHandler) { result.push_negotiation = $0.pointee }
        result.context = context
        return result
    }
}

/// The object the C callbacks reach their sink through.
private final class Sink: @unchecked Sendable {
    let onEvent: @Sendable (GritProgressEvent) -> Void
    init(_ onEvent: @escaping @Sendable (GritProgressEvent) -> Void) { self.onEvent = onEvent }
}

// The shim passes a context through to whichever hook fires, so all seven
// recover the sink the same way. The @convention(c) hooks cannot capture a
// context, which is the only reason this indirection exists; this helper itself
// is ordinary Swift and is only ever called from inside them.
private func sinkFrom(_ context: UnsafeMutableRawPointer?) -> Sink? {
    guard let context else { return nil }
    return Unmanaged<Sink>.fromOpaque(context).takeUnretainedValue()
}

private func text(_ p: UnsafePointer<CChar>?) -> String? {
    guard let p else { return nil }
    return String(cString: p)
}

private let sidebandHandler: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void = { context, message in
    guard let sink = sinkFrom(context), let line = text(message) else { return }
    sink.onEvent(.message(line))
}

private let transferHandler: @convention(c) (
    UnsafeMutableRawPointer?, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt64
) -> Void = { context, _, _, received, _, _, _, bytes in
    guard let sink = sinkFrom(context) else { return }
    // Zero means "not measured", not "nothing arrived". That distinction is why
    // the UI can leave the number out instead of printing 0 B.
    sink.onEvent(.received(objects: Int(received), bytes: bytes == 0 ? nil : bytes))
}

private let updateTipHandler: @convention(c) (
    UnsafeMutableRawPointer?, UnsafePointer<CChar>?, UnsafePointer<CChar>?, UnsafePointer<CChar>?
) -> Void = { context, refname, old, new in
    guard let sink = sinkFrom(context), let name = text(refname) else { return }
    sink.onEvent(.refUpdated(name: name, old: text(old), new: text(new)))
}

private let packHandler: @convention(c) (UnsafeMutableRawPointer?, Int32, UInt32, UInt32) -> Void = { context, stage, _, _ in
    guard let sink = sinkFrom(context) else { return }
    sink.onEvent(.packPhase(building: stage == 0))
}

private let pushTransferHandler: @convention(c) (UnsafeMutableRawPointer?, UInt32, UInt32, UInt64) -> Void = { context, _, _, bytes in
    guard let sink = sinkFrom(context) else { return }
    sink.onEvent(.pushed(bytes: bytes))
}

private let pushUpdateRefHandler: @convention(c) (
    UnsafeMutableRawPointer?, UnsafePointer<CChar>?, UnsafePointer<CChar>?
) -> Void = { context, refname, status in
    guard let sink = sinkFrom(context), let name = text(refname) else { return }
    sink.onEvent(.pushResult(ref: name, reason: text(status)))
}

private let pushNegotiationHandler: @convention(c) (
    UnsafeMutableRawPointer?, UnsafePointer<CChar>?, UnsafePointer<CChar>?
) -> Void = { context, refname, new in
    guard let sink = sinkFrom(context), let name = text(refname), let oid = text(new) else { return }
    sink.onEvent(.pushNegotiated(ref: name, oid: oid))
}
