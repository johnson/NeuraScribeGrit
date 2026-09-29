//  GritProgressBridgeTests — the Swift half of the progress seam.
//
//  These drive the C function pointers directly rather than going near a
//  network. The pointers are the whole contract: grit fills in the struct,
//  calls whichever hook with the context that came back with it, and the Swift
//  side has to turn those raw arguments into the right event. Calling them by
//  hand exercises exactly that, and unlike an integration test it cannot be
//  skipped when a git server happens to be down.

import Foundation
import Testing

#if canImport(neurascribe_gritFFI)
import neurascribe_gritFFI
#endif

@testable import NeuraScribeGrit

/// Runs the seven hooks against a recording bridge and hands back what it saw.
///
/// The struct is built by the production path, so a hook that is wired to the
/// wrong field, or a context that is not threaded through, fails here rather
/// than silently never firing.
private func events(_ body: (GritProgressCallbacks) -> Void) -> [GritProgressEvent] {
    let box = BoxOfEvents()
    let bridge = GritProgressBridge { event in box.events.append(event) }
    GritProgressBridge.withRegistration(bridge) { callbacks in
        body(callbacks)
    }
    return box.events
}

private final class BoxOfEvents: @unchecked Sendable {
    var events: [GritProgressEvent] = []
}

@Suite("Progress events decode from the C hooks")
struct GritProgressBridgeTests {
    @Test("A sideband line arrives as a message")
    func sidebandBecomesAMessage() {
        let seen = events { callbacks in
            "Counting objects: 47%".withCString {
                callbacks.sideband?(callbacks.context, $0)
            }
        }
        #expect(seen == [.message("Counting objects: 47%")])
    }

    @Test("A moved ref carries both OIDs")
    func updateTipCarriesBothOids() {
        let seen = events { callbacks in
            "refs/heads/main".withCString { name in
                "abc123".withCString { old in
                    "def456".withCString { new in
                        callbacks.update_tip?(callbacks.context, name, old, new)
                    }
                }
            }
        }
        #expect(seen == [
            .refUpdated(name: "refs/heads/main", old: "abc123", new: "def456"),
        ])
    }

    @Test("A created ref has no old OID")
    func createdRefHasNoOldOid() {
        let seen = events { callbacks in
            "refs/remotes/origin/main".withCString { name in
                "abc123".withCString { new in
                    callbacks.update_tip?(callbacks.context, name, nil, new)
                }
            }
        }
        #expect(seen == [
            .refUpdated(name: "refs/remotes/origin/main", old: nil, new: "abc123"),
        ])
    }

    @Test("A byte count of zero is absent, not zero bytes")
    func zeroBytesIsAbsent() {
        // The shim cannot measure a local remote, and reports 0 rather than
        // inventing a total. The UI has to be able to tell that apart from
        // "nothing arrived", which is why this is nil and not 0.
        let seen = events { callbacks in
            callbacks.transfer?(callbacks.context, 0, 0, 3, 0, 0, 0, 0)
        }
        #expect(seen == [.received(objects: 3, bytes: nil)])
    }

    @Test("A measured byte count comes through")
    func measuredBytesComeThrough() {
        let seen = events { callbacks in
            callbacks.transfer?(callbacks.context, 0, 0, 12, 0, 0, 0, 4096)
        }
        #expect(seen == [.received(objects: 12, bytes: 4096)])
    }

    @Test("Pack stages are building then uploading")
    func packStagesAreBuildingThenUploading() {
        let seen = events { callbacks in
            callbacks.pack?(callbacks.context, 0, 0, 0)
            callbacks.pack?(callbacks.context, 1, 0, 0)
        }
        #expect(seen == [.packPhase(building: true), .packPhase(building: false)])
    }

    @Test("An accepted push has no rejection reason")
    func acceptedPushHasNoReason() {
        let seen = events { callbacks in
            "refs/heads/main".withCString { name in
                callbacks.push_update_ref?(callbacks.context, name, nil)
            }
        }
        #expect(seen == [.pushResult(ref: "refs/heads/main", reason: nil)])
    }

    @Test("A rejected push carries the server's reason")
    func rejectedPushCarriesReason() {
        let seen = events { callbacks in
            "refs/heads/main".withCString { name in
                "non-fast-forward".withCString { reason in
                    callbacks.push_update_ref?(callbacks.context, name, reason)
                }
            }
        }
        #expect(seen == [
            .pushResult(ref: "refs/heads/main", reason: "non-fast-forward"),
        ])
    }

    @Test("The negotiated update reports the local tip")
    func negotiationReportsLocalTip() {
        let seen = events { callbacks in
            "refs/heads/main".withCString { name in
                "abc123".withCString { oid in
                    callbacks.push_negotiation?(callbacks.context, name, oid)
                }
            }
        }
        #expect(seen == [.pushNegotiated(ref: "refs/heads/main", oid: "abc123")])
    }

    @Test("The upload reports the bytes it sent")
    func uploadReportsBytesSent() {
        let seen = events { callbacks in
            callbacks.push_transfer?(callbacks.context, 0, 0, 2048)
        }
        #expect(seen == [.pushed(bytes: 2048)])
    }

    @Test("The silent bridge reports nothing and stays callable")
    func silentBridgeIsHarmless() {
        GritProgressBridge.withRegistration(.silent) { callbacks in
            // A NULL context is what a caller with nothing to report passes; the
            // hooks have to shrug rather than trap.
            callbacks.sideband?(callbacks.context, nil)
            callbacks.update_tip?(callbacks.context, nil, nil, nil)
            callbacks.transfer?(callbacks.context, 0, 0, 0, 0, 0, 0, 0)
        }
    }
}
