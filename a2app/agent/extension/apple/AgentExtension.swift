//! Robrix's ExtensionFoundation app extension.
//!
//! A thin Swift shell: it accepts the host's single XPC connection and
//! multiplexes many logical sessions over it — one Octos host-managed agent
//! loop (`octos_host_managed_*`, linked in-process as `liboctos_ffi.dylib`)
//! per room. No provider credential or policy authority lives here; every
//! model completion and tool call is brokered back to Robrix over the same
//! XPC connection, exactly as the stdin/stdout confined child does.
//!
//! `__HOST_BUNDLE_ID__` is substituted by `build.sh` so the `@Bind` binding
//! and the generated Info.plist match the host app the extension is embedded
//! in (the dev bundle uses a different identifier than a release build).

import Foundation
import ExtensionFoundation
import Darwin

typealias HMSend = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int) -> Int32
typealias HMStart = @convention(c) (UInt32, UnsafePointer<CChar>?, HMSend?, UnsafeMutableRawPointer?) -> UnsafeMutableRawPointer?
typealias HMFeed = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int) -> Int32
typealias HMFree = @convention(c) (UnsafeMutableRawPointer?) -> Void

private var hmStart: HMStart?
private var hmFeed: HMFeed?
private var hmFree: HMFree?

/// Loads the bundled Octos host-managed FFI. Fails soft: without it sessions
/// simply cannot serve, and the host reports the connection error.
private func loadOctos() -> Bool {
    guard let dir = Bundle.main.privateFrameworksPath else { return false }
    guard let handle = dlopen(dir + "/liboctos_ffi.dylib", RTLD_NOW) else { return false }
    hmStart = unsafeBitCast(dlsym(handle, "octos_host_managed_start"), to: HMStart?.self)
    hmFeed = unsafeBitCast(dlsym(handle, "octos_host_managed_feed"), to: HMFeed?.self)
    hmFree = unsafeBitCast(dlsym(handle, "octos_host_managed_free"), to: HMFree?.self)
    return hmStart != nil && hmFeed != nil && hmFree != nil
}

/// The single connection protocol. The first three methods are implemented by
/// the extension (host → extension); `deliverFrame` is implemented by the host
/// (extension → host). Both ends must declare it identically.
@objc protocol RobrixAgentXPC {
    func openSession(_ id: UInt64)
    func sendFrame(_ id: UInt64, _ frame: Data)
    func closeSession(_ id: UInt64)
    func deliverFrame(_ id: UInt64, _ frame: Data)
}

/// One Octos host-managed agent loop for one room.
final class AgentReceiver {
    let id: UInt64
    weak var router: SessionRouter?
    private var handle: UnsafeMutableRawPointer?
    private var context: UnsafeMutableRawPointer?
    private var started = false

    init(id: UInt64, router: SessionRouter) {
        self.id = id
        self.router = router
    }

    func start() {
        guard !started, loadOctos(), let hmStart else {
            NSLog("RobrixAgent: session %llu loadOctos failed", id)
            return
        }
        started = true
        // Retained (never released) so an Octos callback can never dereference
        // a freed receiver after the session closes. One small leak per room
        // session, bounded by the extension process's lifetime.
        let context = Unmanaged.passRetained(self).toOpaque()
        self.context = context
        handle = hmStart(20, "xpc-extension", agentSend, context)
        NSLog("RobrixAgent: session %llu handle=%@", id, handle == nil ? "nil" : "ok")
    }

    func stop() {
        if let handle, let hmFree { hmFree(handle) }
        handle = nil
        started = false
    }

    func feed(_ frame: Data) {
        guard let handle, let hmFeed else {
            NSLog("RobrixAgent: session %llu has no octos handle", id)
            return
        }
        frame.withUnsafeBytes { raw in
            _ = hmFeed(handle, raw.bindMemory(to: UInt8.self).baseAddress, frame.count)
        }
    }

    /// Octos → host, tagged with this session's id.
    func forward(_ frame: Data) {
        router?.forward(id, frame)
    }
}

/// Top-level C callback so `octos_host_managed_start` gets a function pointer.
private func agentSend(ctx: UnsafeMutableRawPointer?, data: UnsafePointer<UInt8>?, len: Int) -> Int32 {
    guard let ctx, let data else { return 0 }
    let receiver = Unmanaged<AgentReceiver>.fromOpaque(ctx).takeUnretainedValue()
    receiver.forward(Data(bytes: data, count: len))
    return 1
}

/// Routes frames between the one XPC connection and its per-room sessions.
final class SessionRouter: NSObject, RobrixAgentXPC, @unchecked Sendable {
    weak var connection: NSXPCConnection?
    private var sessions: [UInt64: AgentReceiver] = [:]
    private let lock = NSLock()

    func openSession(_ id: UInt64) {
        NSLog("RobrixAgent: openSession %llu", id)
        let receiver = AgentReceiver(id: id, router: self)
        lock.lock(); sessions[id] = receiver; lock.unlock()
        receiver.start()
    }

    func sendFrame(_ id: UInt64, _ frame: Data) {
        lock.lock(); let receiver = sessions[id]; lock.unlock()
        receiver?.feed(frame)
    }

    func closeSession(_ id: UInt64) {
        NSLog("RobrixAgent: closeSession %llu", id)
        lock.lock(); let receiver = sessions.removeValue(forKey: id); lock.unlock()
        receiver?.stop()
    }

    /// Implemented by the host; unused here.
    func deliverFrame(_ id: UInt64, _ frame: Data) {}

    func forward(_ id: UInt64, _ frame: Data) {
        (connection?.remoteObjectProxy as? RobrixAgentXPC)?.deliverFrame(id, frame)
    }

    func closeAll() {
        lock.lock(); let all = Array(sessions.values); sessions.removeAll(); lock.unlock()
        for receiver in all { receiver.stop() }
    }
}

final class AgentConfiguration: AppExtensionConfiguration, @unchecked Sendable {
    func accept(connection: NSXPCConnection) -> Bool {
        NSLog("RobrixAgent: accept connection")
        let router = SessionRouter()
        router.connection = connection
        connection.invalidationHandler = { [weak router] in router?.closeAll() }
        connection.exportedInterface = NSXPCInterface(with: RobrixAgentXPC.self)
        connection.exportedObject = router
        connection.remoteObjectInterface = NSXPCInterface(with: RobrixAgentXPC.self)
        connection.resume()
        return true
    }
}

@main
class RobrixAgentExtension: AppExtension {
    @AppExtensionPoint.Bind
    var boundPoint: AppExtensionPoint {
        AppExtensionPoint.Identifier(host: "__HOST_BUNDLE_ID__", name: "agent-host")
    }
    required init() { NSLog("RobrixAgent: init") }
    let config = AgentConfiguration()
    var configuration: some AppExtensionConfiguration {
        NSLog("RobrixAgent: configuration requested")
        return config
    }
}
