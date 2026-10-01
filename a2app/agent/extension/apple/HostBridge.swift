//! Host side of the ExtensionFoundation app-extension bridge.
//!
//! Built as `libRobrixExtensionHost.dylib` and `dlopen`ed by Rust
//! (`a2app-agent`'s Apple bridge loader). It declares Robrix's extension
//! point, discovers the installed `.appex`, and runs **one** shared extension
//! process whose single XPC connection multiplexes a host-managed Octos
//! session per room.
//!
//! `HostManager` owns the process and connection. `HostSession` is one room's
//! logical channel over it: Rust gets one session handle per
//! `robrix_ext_launch`, and frames are tagged with the session id so rooms
//! never see each other's traffic. The `AppExtensionProcess` is kept alive for
//! the app's lifetime — letting it deallocate would let the system invalidate
//! the shared process and interrupt every room.

import Foundation
import ExtensionFoundation

@available(macOS 26.0, *)
extension AppExtensionPoint {
    @Definition
    public static var robrixAgentHost: AppExtensionPoint {
        Name("agent-host")
        Scope(restriction: .application)
        UserInterface(false)
    }
}

@objc protocol RobrixAgentXPC {
    func openSession(_ id: UInt64)
    func sendFrame(_ id: UInt64, _ frame: Data)
    func closeSession(_ id: UInt64)
    func deliverFrame(_ id: UInt64, _ frame: Data)
}

/// Owns the one extension process and its connection; hands out `HostSession`s.
@available(macOS 26.0, *)
final class HostManager: NSObject, RobrixAgentXPC, @unchecked Sendable {
    static let shared = HostManager()

    private let condition = NSCondition()
    private var resolved: AppExtensionIdentity?
    private var discoveryDone = false
    private var discoveryStarted = false
    private var process: AppExtensionProcess?
    private var connection: NSXPCConnection?
    private var proxy: RobrixAgentXPC?
    private var sessions: [UInt64: HostSession] = [:]
    private var nextId: UInt64 = 1

    func discover() -> Bool {
        ensureDiscovery()
        condition.lock()
        while !discoveryDone { condition.wait() }
        let found = resolved != nil
        condition.unlock()
        return found
    }

    private func ensureDiscovery() {
        condition.lock()
        if discoveryStarted { condition.unlock(); return }
        discoveryStarted = true
        condition.unlock()
        Task { [weak self] in
            var found: AppExtensionIdentity?
            do {
                let monitor = try await AppExtensionPoint.Monitor(appExtensionPoint: .robrixAgentHost)
                // `identities` is populated asynchronously; give it a moment
                // rather than treating the first empty read as "not installed".
                for attempt in 0..<15 {
                    let state = monitor.state
                    if let first = monitor.identities.first {
                        found = first
                        break
                    }
                    if attempt == 0 || attempt == 14 {
                        NSLog(
                            "RobrixExtensionHost: monitor identities=%d unapproved=%d disabled=%d",
                            state.identities.count, state.unapprovedCount, state.disabledCount
                        )
                    }
                    try? await Task.sleep(nanoseconds: 200_000_000)
                }
            } catch {
                NSLog("RobrixExtensionHost: monitor error: %@", String(describing: error))
                found = nil
            }
            self?.finishDiscovery(found)
        }
    }

    /// Synchronous so `NSCondition.lock()` is not called from an async context.
    private func finishDiscovery(_ found: AppExtensionIdentity?) {
        condition.lock()
        resolved = found
        discoveryDone = true
        condition.broadcast()
        condition.unlock()
    }

    /// Lazily launches (or reconnects to) the single extension process.
    private func ensureConnection() -> Bool {
        ensureDiscovery()
        condition.lock()
        while !discoveryDone { condition.wait() }
        if proxy != nil { condition.unlock(); return true }
        guard let identity = resolved else { condition.unlock(); return false }
        condition.unlock()
        do {
            let configuration = AppExtensionProcess.Configuration(appExtensionIdentity: identity) { [weak self] in
                NSLog("RobrixExtensionHost: extension process interrupted")
                self?.interrupted()
            }
            let process = try AppExtensionProcess(configuration: configuration)
            let connection = try process.makeXPCConnection()
            connection.exportedInterface = NSXPCInterface(with: RobrixAgentXPC.self)
            connection.exportedObject = self
            connection.remoteObjectInterface = NSXPCInterface(with: RobrixAgentXPC.self)
            connection.resume()
            condition.lock()
            self.process = process
            self.connection = connection
            self.proxy = connection.remoteObjectProxy as? RobrixAgentXPC
            condition.broadcast()
            condition.unlock()
            NSLog("RobrixExtensionHost: connected")
            return true
        } catch {
            NSLog("RobrixExtensionHost: connect failed: %@", String(describing: error))
            return false
        }
    }

    /// Opens one room session over the shared connection.
    func launch() -> HostSession? {
        guard ensureConnection(), let proxy else { return nil }
        condition.lock()
        let id = nextId
        nextId += 1
        let session = HostSession(id: id, manager: self)
        sessions[id] = session
        condition.unlock()
        proxy.openSession(id)
        NSLog("RobrixExtensionHost: openSession %llu", id)
        return session
    }

    func send(_ id: UInt64, _ frame: Data) -> Bool {
        condition.lock()
        let proxy = self.proxy
        condition.unlock()
        guard let proxy else { return false }
        proxy.sendFrame(id, frame)
        return true
    }

    func close(_ id: UInt64) {
        condition.lock()
        sessions.removeValue(forKey: id)
        let proxy = self.proxy
        condition.unlock()
        proxy?.closeSession(id)
    }

    func deliverFrame(_ id: UInt64, _ frame: Data) {
        condition.lock()
        let session = sessions[id]
        condition.unlock()
        session?.enqueue(frame)
    }

    // Implemented by the extension; unused here.
    func openSession(_ id: UInt64) {}
    func sendFrame(_ id: UInt64, _ frame: Data) {}
    func closeSession(_ id: UInt64) {}

    /// The shared process died: close every room session so Rust sees EOF, and
    /// allow a later launch to reconnect.
    private func interrupted() {
        condition.lock()
        let all = Array(sessions.values)
        sessions.removeAll()
        process = nil
        connection = nil
        proxy = nil
        condition.unlock()
        for session in all { session.markClosed() }
    }
}

/// One room's logical channel over the shared connection.
@available(macOS 26.0, *)
final class HostSession: NSObject, @unchecked Sendable {
    let id: UInt64
    private let manager: HostManager
    private let condition = NSCondition()
    private var inbox: [Data] = []
    private var closed = false
    private var released = false
    private var activeCalls = 0

    init(id: UInt64, manager: HostManager) {
        self.id = id
        self.manager = manager
        super.init()
    }

    func enqueue(_ frame: Data) {
        condition.lock(); inbox.append(frame); condition.broadcast(); condition.unlock()
    }

    func markClosed() {
        condition.lock(); closed = true; condition.broadcast(); condition.unlock()
    }

    private func beginCall() -> Bool {
        condition.lock()
        let ok = !released
        if ok { activeCalls += 1 }
        condition.unlock()
        return ok
    }

    private func endCall() {
        condition.lock()
        activeCalls -= 1
        if released && activeCalls == 0 { condition.broadcast() }
        condition.unlock()
    }

    func send(_ frame: Data) -> Bool {
        guard beginCall() else { return false }
        defer { endCall() }
        condition.lock()
        let closed = self.closed
        condition.unlock()
        guard !closed else { return false }
        return manager.send(id, frame)
    }

    /// Blocks until a frame arrives or the session closes.
    func recv() -> Data? {
        guard beginCall() else { return nil }
        defer { endCall() }
        condition.lock()
        while inbox.isEmpty && !closed { condition.wait() }
        let frame = inbox.isEmpty ? nil : inbox.removeFirst()
        condition.unlock()
        return frame
    }

    /// Closes this room's session (never the shared process), waits for any
    /// in-flight call, then lets the caller release the retained reference.
    func freeAndRelease() {
        condition.lock()
        released = true
        closed = true
        condition.broadcast()
        while activeCalls > 0 { condition.wait() }
        condition.unlock()
        manager.close(id)
    }
}

// MARK: - C ABI (called from Rust)

@_cdecl("robrix_ext_available")
public func robrix_ext_available() -> Int32 {
    guard #available(macOS 26.0, *) else { return 0 }
    return HostManager.shared.discover() ? 1 : 0
}

@_cdecl("robrix_ext_launch")
public func robrix_ext_launch() -> UnsafeMutableRawPointer? {
    guard #available(macOS 26.0, *), let session = HostManager.shared.launch() else { return nil }
    return Unmanaged.passRetained(session).toOpaque()
}

@_cdecl("robrix_ext_send")
public func robrix_ext_send(
    _ handle: UnsafeMutableRawPointer?,
    _ data: UnsafePointer<UInt8>?,
    _ len: Int
) -> Int32 {
    guard #available(macOS 26.0, *), let handle, let data else { return 0 }
    let session = Unmanaged<HostSession>.fromOpaque(handle).takeUnretainedValue()
    return session.send(Data(bytes: data, count: len)) ? 1 : 0
}

/// Blocks for the next frame on this session. On success returns 1 and sets
/// `*out`/`*outLen` to a malloc'd buffer the caller frees with
/// `robrix_ext_free_bytes`; 0 means the session closed.
@_cdecl("robrix_ext_recv")
public func robrix_ext_recv(
    _ handle: UnsafeMutableRawPointer?,
    _ out: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>?,
    _ outLen: UnsafeMutablePointer<Int>?
) -> Int32 {
    guard #available(macOS 26.0, *), let handle, let out, let outLen else { return 0 }
    let session = Unmanaged<HostSession>.fromOpaque(handle).takeUnretainedValue()
    guard let frame = session.recv() else { return 0 }
    let buffer = UnsafeMutablePointer<UInt8>.allocate(capacity: frame.count)
    frame.copyBytes(to: buffer, count: frame.count)
    out.pointee = buffer
    outLen.pointee = frame.count
    return 1
}

/// Closes and releases one session. NULL is ignored.
@_cdecl("robrix_ext_free")
public func robrix_ext_free(_ handle: UnsafeMutableRawPointer?) {
    guard #available(macOS 26.0, *), let handle else { return }
    let unmanaged = Unmanaged<HostSession>.fromOpaque(handle)
    unmanaged.takeUnretainedValue().freeAndRelease()
    unmanaged.release()
}

@_cdecl("robrix_ext_free_bytes")
public func robrix_ext_free_bytes(_ buffer: UnsafeMutablePointer<UInt8>?) {
    buffer?.deallocate()
}
