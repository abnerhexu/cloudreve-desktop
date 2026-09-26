import Foundation
import FileProvider
import Security

// Production uses a shared data-protection Keychain group. Local ad-hoc builds
// use the login Keychain with an ACL limited to the two signed executables.
// No credentials in Info.plist, argv, defaults or logs.
enum Credentials {
    static var localKeychain: Bool { Bundle.main.object(forInfoDictionaryKey: "CloudreveLocalKeychain") as? Bool == true }
    static func check(_ status: OSStatus) throws {
        guard status == errSecSuccess else { throw NSError(domain: NSOSStatusErrorDomain, code: Int(status)) }
    }
    static func localAccess() throws -> SecAccess {
        let bundle = Bundle.main.bundleURL
        let host = bundle.pathExtension == "appex" ? bundle.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent() : bundle
        let paths = [host.appendingPathComponent("Contents/MacOS/cloudreve-provider-host"),
                     host.appendingPathComponent("Contents/PlugIns/CloudreveFileProvider.appex/Contents/MacOS/CloudreveFileProvider")]
        var trusted: [SecTrustedApplication] = []
        for path in paths {
            var app: SecTrustedApplication?
            try check(SecTrustedApplicationCreateFromPath(path.path, &app))
            guard let app else { throw CocoaError(.coderInvalidValue) }
            trusted.append(app)
        }
        var access: SecAccess?
        try check(SecAccessCreate("Cloudreve local Finder account" as CFString, trusted as CFArray, &access))
        guard let access else { throw CocoaError(.coderInvalidValue) }
        return access
    }
    static func query(_ domain: String) throws -> [String: Any] {
        guard UUID(uuidString: domain) != nil else { throw CocoaError(.fileReadInvalidFileName) }
        if localKeychain {
            return [kSecClass as String: kSecClassGenericPassword,
                    kSecAttrService as String: "cloudreve.file-provider.local",
                    kSecAttrAccount as String: domain,
                    kSecUseDataProtectionKeychain as String: false]
        }
        guard let group = Bundle.main.object(forInfoDictionaryKey: "CloudreveKeychainGroup") as? String,
              !group.contains("$(") else { throw NSFileProviderError(.notAuthenticated) }
        return [kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: "cloudreve.file-provider",
                kSecAttrAccount as String: domain,
                kSecAttrAccessGroup as String: group,
                kSecUseDataProtectionKeychain as String: true]
    }
    static func load(_ domain: String) throws -> DomainConfiguration {
        var query = try query(domain)
        query[kSecReturnData as String] = true
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { throw NSFileProviderError(.notAuthenticated) }
        try check(status)
        guard let data = result as? Data else { throw CocoaError(.coderInvalidValue) }
        return try JSONDecoder().decode(DomainConfiguration.self, from: data)
    }
    static func remove(_ domain: String) throws {
        let status = SecItemDelete(try query(domain) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw NSFileProviderError(.notAuthenticated) }
    }
    static func save(_ config: DomainConfiguration) throws {
        var query = try query(config.domain)
        let attributes: [String: Any] = [kSecValueData as String: try JSONEncoder().encode(config)]
        let status = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        if status == errSecItemNotFound {
            query.merge(attributes) { _, new in new }
            if localKeychain { query[kSecAttrAccess as String] = try localAccess() }
            else { query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly }
            try check(SecItemAdd(query as CFDictionary, nil))
        } else { try check(status) }
    }
}

final class Call {
    let progress = Progress(totalUnitCount: 1000)
    private let lock = NSLock()
    private var operation: OpaquePointer?
    init() {
        progress.cancellationHandler = { [weak self] in
            guard let self else { return }
            self.lock.lock(); defer { self.lock.unlock() }
            if let op = self.operation { crfp_cancel(op) }
        }
    }
    func run(_ request: Data) throws -> Data {
        lock.lock()
        if progress.isCancelled { lock.unlock(); throw CocoaError(.userCancelled) }
        let op = String(decoding: request, as: UTF8.self).withCString { crfp_create($0) }
        operation = op
        lock.unlock()
        guard let op else { throw CocoaError(.coderInvalidValue) }
        let timer = DispatchSource.makeTimerSource(queue: .global(qos: .utility))
        timer.schedule(deadline: .now(), repeating: .milliseconds(200))
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            self.lock.lock(); defer { self.lock.unlock() }
            if let op = self.operation { self.progress.completedUnitCount = Int64(min(crfp_progress(op), 1000)) }
        }
        timer.resume()
        let result = crfp_run(op)
        timer.cancel()
        lock.lock(); operation = nil; crfp_free(op); lock.unlock()
        guard let result else { throw CocoaError(.coderInvalidValue) }
        defer { crfp_string_free(result) }
        return Data(String(cString: result).utf8)
    }
}

final class Backend {
    // Serializes credential rotation, calls, and snapshot bookkeeping per domain.
    let queue = DispatchQueue(label: "cloudreve.file-provider")
    let domain: NSFileProviderDomain
    let manager: NSFileProviderManager
    init(domain: NSFileProviderDomain) {
        self.domain = domain
        self.manager = NSFileProviderManager(for: domain)!
    }
    var writable: Bool { (try? Credentials.load(domain.identifier.rawValue).writable) == true }
    func stateDirectory() throws -> URL {
        guard UUID(uuidString: domain.identifier.rawValue) != nil else { throw CocoaError(.fileReadInvalidFileName) }
        let base = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
        let path = base.appendingPathComponent("CloudreveMutations").appendingPathComponent(domain.identifier.rawValue)
        try FileManager.default.createDirectory(at: path, withIntermediateDirectories: true)
        return path
    }
    func request<T: Decodable>(_ operation: String, _ fields: [String: Any], call: Call, as: T.Type) throws -> T {
        var config = try Credentials.load(domain.identifier.rawValue)
        var object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(config)) as! [String: Any]
        object["operation"] = operation
        object["state_directory"] = try stateDirectory().path
        object.merge(fields) { _, new in new }
        let data = try call.run(JSONSerialization.data(withJSONObject: object))
        let result = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        if let tokens = result["tokens"] as? [String: String], tokens != config.tokens {
            config.tokens = tokens
            try Credentials.save(config)
        }
        if let error = result["error"] as? String {
            switch error {
            case "cancelled": throw CocoaError(.userCancelled)
            case "not_found": throw NSFileProviderError(.noSuchItem)
            case "not_authenticated": throw NSFileProviderError(.notAuthenticated)
            case "unreachable": throw NSFileProviderError(.serverUnreachable)
            case "permission": throw CocoaError(.fileWriteNoPermission)
            case "version_conflict":
                manager.signalEnumerator(for: .workingSet) { _ in }
                throw NSFileProviderError(.cannotSynchronize)
            case "collision": throw CocoaError(.fileWriteFileExists)
            case "directory_not_empty": throw NSFileProviderError(.directoryNotEmpty)
            default: throw CocoaError(.coderInvalidValue)
            }
        }
        if let object = result["data"] as? [String: Any], object["version_unavailable"] as? Bool == true {
            throw NSFileProviderError(.versionNoLongerAvailable)
        }
        return try JSONDecoder().decode(T.self, from: JSONSerialization.data(withJSONObject: result["data"] as Any, options: [.fragmentsAllowed]))
    }
    func remote(_ id: NSFileProviderItemIdentifier, call: Call) throws -> RemoteFile {
        try request("item", ["id": id == .rootContainer ? "root" : id.rawValue], call: call, as: RemoteFile.self)
    }
    func item(_ id: NSFileProviderItemIdentifier, call: Call) throws -> Item {
        let file = try remote(id, call: call)
        if id == .rootContainer { return Item(file, id: .rootContainer, parent: .rootContainer, writable: writable) }
        let config = try Credentials.load(domain.identifier.rawValue)
        let parentPath = String(file.path.prefix(upTo: file.path.lastIndex(of: "/")!))
        if parentPath.trimmingCharacters(in: CharacterSet(charactersIn: "/")) == config.root.trimmingCharacters(in: CharacterSet(charactersIn: "/")) {
            return Item(file, parent: .rootContainer, writable: writable)
        }
        // Parent resolution uses the authoritative folder hierarchy, not persisted
        // filename-derived IDs (which would change on rename).
        let all = try snapshot(call: call)
        guard let parent = all.first(where: { $0.remote.path == parentPath }) else { throw NSFileProviderError(.serverUnreachable) }
        return Item(file, parent: parent.itemIdentifier, writable: writable)
    }
    func children(_ id: NSFileProviderItemIdentifier, call: Call) throws -> [Item] {
        let files: [RemoteFile] = try request("list", ["id": id == .rootContainer ? "root" : id.rawValue], call: call, as: [RemoteFile].self)
        return files.map { Item($0, parent: id, writable: writable) }
    }
    func snapshot(call: Call) throws -> [Item] {
        var pending: [NSFileProviderItemIdentifier] = [.rootContainer]
        var seen = Set<NSFileProviderItemIdentifier>()
        var result = [Item]()
        while let id = pending.popLast() {
            guard seen.insert(id).inserted else { throw CocoaError(.coderInvalidValue) }
            let items = try children(id, call: call)
            result += items
            pending += items.filter { $0.remote.type == 1 }.map(\.itemIdentifier)
        }
        return result
    }
}
