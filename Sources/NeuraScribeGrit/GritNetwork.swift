//  GritNetwork — clone, fetch and push through grit.
//
//  Replaces the MiniGit/libgit2 path. The protocol is grit-lib's own smart-HTTP
//  transport, which speaks v0/v1; GritHttpBridge supplies URLSession underneath.
//
//  Progress: GritProgressBridge carries the seven hooks. They are not equally well
//  informed, and GritProgressEvent says which numbers are real and which are
//  absent (nil, not 0) — so a caller must not assume every hook fires.

import Foundation

#if canImport(neurascribe_gritFFI)
import neurascribe_gritFFI
#endif

/// The outcome of a push for one ref.
public struct GritPushResult: Decodable, Hashable, Sendable {
    /// Fully qualified remote ref, for example `refs/heads/main`.
    public let name: String
    /// `ok` when the ref moved, `up_to_date` when it was already there,
    /// `rejected` when the server refused it.
    public let status: String
    /// The server's explanation for a rejection; nil otherwise.
    public let reason: String?

    public var succeeded: Bool { status == "ok" || status == "up_to_date" }
}

/// The outcome of a fetch.
///
/// grit reports what moved as a `ref name -> oid` map rather than a list of
/// before/after pairs, so this cannot say what a ref used to be. Callers that
/// need the old value — the pull path does — have to read it first.
public struct GritFetchResult: Decodable, Hashable, Sendable {
    /// Refs the server had, for the branches that were asked for.
    public let updatedRefs: [String: String]
    /// The remote's default branch, when the server advertised one.
    public let defaultBranch: String?

    enum CodingKeys: String, CodingKey {
        case updatedRefs = "updated_refs"
        case defaultBranch = "default_branch"
    }
}

/// Network operations against a remote.
public enum GritNetwork {
    /// Clone `url` into `destination` and return the branch that was checked out.
    ///
    /// - Parameters:
    ///   - branch: which branch to check out; nil takes whatever HEAD points at.
    ///   - credential: sent after a 401. Safe to pass nil for a public repo —
    ///     the shim only retries once it has actually seen a challenge.
    public static func clone(
        url: String,
        to destination: String,
        branch: String? = nil,
        credential: GritCredential? = nil,
        http: GritHttpBridge = GritHttpBridge(),
        progress: GritProgressBridge = .silent
    ) throws -> String {
        let raw = GritProgressBridge.withRegistration(progress) { progressCallbacks in
            withUnsafePointer(to: progressCallbacks) { progress in
                url.withCString { urlPointer in
                    destination.withCString { destPointer in
                        let branchPointer = branch?.withCString { UnsafePointer($0) }
                        let credentialPointer = credential?.shimJSON?.withCString { UnsafePointer($0) }
                        return withUnsafePointer(to: http.callbacks) { callbacks in
                            grit_clone(
                                urlPointer, destPointer, branchPointer, credentialPointer,
                                callbacks, progress
                            )
                        }
                    }
                }
            }
        }
        return try take(raw)
    }

    /// Fetch from `url` into an open repository.
    ///
    /// - Parameter refspecs: for example `["+refs/heads/*:refs/remotes/origin/*"]`.
    ///   grit has no "an empty refspec means everything" mode, so the wildcard
    ///   has to be spelled out; an empty list fetches nothing at all.
    public static func fetch(
        repository: GritRepository,
        url: String,
        refspecs: [String],
        credential: GritCredential? = nil,
        http: GritHttpBridge = GritHttpBridge(),
        progress: GritProgressBridge = .silent
    ) throws -> GritFetchResult {
        let encoded = (try? String(decoding: JSONEncoder().encode(refspecs), as: UTF8.self)) ?? "[]"
        let raw = GritProgressBridge.withRegistration(progress) { progressCallbacks in
            withUnsafePointer(to: progressCallbacks) { progress in
                url.withCString { urlPointer in
                    encoded.withCString { refspecPointer in
                        let credentialPointer = credential?.shimJSON?.withCString { UnsafePointer($0) }
                        return withUnsafePointer(to: http.callbacks) { callbacks in
                            grit_repo_fetch_json(
                                repository.rawHandle, urlPointer, refspecPointer,
                                credentialPointer, callbacks, progress
                            )
                        }
                    }
                }
            }
        }
        return try decode(GritFetchResult.self, from: raw)
    }

    /// Push `branch` to the same-named branch on `url`.
    public static func push(
        repository: GritRepository,
        url: String,
        branch: String,
        force: Bool = false,
        credential: GritCredential? = nil,
        http: GritHttpBridge = GritHttpBridge(),
        progress: GritProgressBridge = .silent
    ) throws -> [GritPushResult] {
        let raw = GritProgressBridge.withRegistration(progress) { progressCallbacks in
            withUnsafePointer(to: progressCallbacks) { progress in
                url.withCString { urlPointer in
                    branch.withCString { branchPointer in
                        let credentialPointer = credential?.shimJSON?.withCString { UnsafePointer($0) }
                        return withUnsafePointer(to: http.callbacks) { callbacks in
                            grit_repo_push_json(
                                repository.rawHandle,
                                urlPointer,
                                branchPointer,
                                force ? 1 : 0,
                                credentialPointer,
                                callbacks,
                                progress
                            )
                        }
                    }
                }
            }
        }
        // The shim wraps the per-ref results in an envelope; unwrap it here so
        // callers get a plain array.
        return try decode(Envelope.self, from: raw).refs
    }

    private struct Envelope: Decodable {
        let refs: [GritPushResult]
    }

    // MARK: - C plumbing

    private static func take(_ raw: UnsafeMutablePointer<CChar>?) throws -> String {
        guard let raw else { throw GritError.lastFailed }
        defer { grit_string_free(raw) }
        return String(cString: raw)
    }

    private static func decode<T: Decodable>(_ type: T.Type, from raw: UnsafeMutablePointer<CChar>?) throws -> T {
        guard let raw else { throw GritError.lastFailed }
        defer { grit_string_free(raw) }
        do {
            return try JSONDecoder().decode(T.self, from: Data(String(cString: raw).utf8))
        } catch {
            throw GritError.malformedResponse(String(cString: raw))
        }
    }
}
