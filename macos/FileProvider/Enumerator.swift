import Foundation
import FileProvider

final class Enumerator: NSObject, NSFileProviderEnumerator {
    let backend: Backend
    let container: NSFileProviderItemIdentifier
    private let call = Call()
    init(_ backend: Backend, container: NSFileProviderItemIdentifier) {
        self.backend = backend
        self.container = container
    }
    func invalidate() { call.progress.cancel() }
    private func items() throws -> [Item] {
        if container == .workingSet { return try backend.snapshot(call: call) }
        let remote = try backend.remote(container, call: call)
        if remote.type == 1 { return try backend.children(container, call: call) }
        return [try backend.item(container, call: call)]
    }
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        backend.queue.async {
            do {
                guard page.rawValue == NSFileProviderPage.initialPageSortedByDate as Data || page.rawValue == NSFileProviderPage.initialPageSortedByName as Data else { throw NSFileProviderError(.pageExpired) }
                let items = try self.items()
                observer.didEnumerate(items)
                observer.finishEnumerating(upTo: nil)
            } catch { observer.finishEnumeratingWithError(error) }
        }
    }
    private func cache() throws -> URL {
        // Hash-like UUID domain IDs are controlled by the host, but validate before
        // constructing a path to prevent accidental scope escape.
        let id = backend.domain.identifier.rawValue
        guard UUID(uuidString: id) != nil else { throw CocoaError(.fileReadInvalidFileName) }
        let base = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
        let url = base.appendingPathComponent("CloudreveAnchors").appendingPathComponent(id)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
    private func save(_ items: [Item]) throws -> NSFileProviderSyncAnchor {
        let id = UUID().uuidString
        var records = [String: String]()
        for item in items {
            guard records[item.itemIdentifier.rawValue] == nil else { throw CocoaError(.coderInvalidValue) }
            records[item.itemIdentifier.rawValue] = item.itemVersion.contentVersion.base64EncodedString() + ":" + item.itemVersion.metadataVersion.base64EncodedString()
        }
        let base = try cache()
        try JSONEncoder().encode(records).write(to: base.appendingPathComponent(id), options: .atomic)
        // Preserve recent anchors for crash/retry. Expired anchors trigger a full
        // re-enumeration; failure never turns the working set into an empty set.
        let files = try FileManager.default.contentsOfDirectory(at: base, includingPropertiesForKeys: [.contentModificationDateKey])
        let sorted = try files.sorted { try $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate ?? .distantPast > $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate ?? .distantPast }
        for old in sorted.dropFirst(8) { try? FileManager.default.removeItem(at: old) }
        return NSFileProviderSyncAnchor(Data(id.utf8))
    }
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        backend.queue.async {
            do { completionHandler(try self.save(self.items())) }
            catch { completionHandler(nil) }
        }
    }
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        backend.queue.async {
            do {
                guard let id = String(data: anchor.rawValue, encoding: .utf8), UUID(uuidString: id) != nil,
                      let data = try? Data(contentsOf: self.cache().appendingPathComponent(id)),
                      let old = try? JSONDecoder().decode([String: String].self, from: data)
                else { throw NSFileProviderError(.syncAnchorExpired) }
                let items = try self.items()
                let present = Set(items.map { $0.itemIdentifier.rawValue })
                let changed = items.filter { old[$0.itemIdentifier.rawValue] != $0.itemVersion.contentVersion.base64EncodedString() + ":" + $0.itemVersion.metadataVersion.base64EncodedString() }
                let next = try self.save(items)
                observer.didUpdate(changed)
                observer.didDeleteItems(withIdentifiers: old.keys.filter { !present.contains($0) }.map { NSFileProviderItemIdentifier($0) })
                observer.finishEnumeratingChanges(upTo: next, moreComing: false)
            } catch { observer.finishEnumeratingWithError(error) }
        }
    }
}
