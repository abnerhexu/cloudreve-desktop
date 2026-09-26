import Foundation
import FileProvider
import UniformTypeIdentifiers

struct RemoteFile: Codable {
    let id: String
    let name: String
    let path: String
    let type: Int
    let size: Int64
    let primary_entity: String?
    let updated_at: String
    let created_at: String
}

final class Item: NSObject, NSFileProviderItem {
    let writable: Bool
    let remote: RemoteFile
    let itemIdentifier: NSFileProviderItemIdentifier
    let parentItemIdentifier: NSFileProviderItemIdentifier
    init(_ remote: RemoteFile, id: NSFileProviderItemIdentifier? = nil, parent: NSFileProviderItemIdentifier, writable: Bool = false) {
        self.writable = writable
        self.remote = remote
        itemIdentifier = id ?? NSFileProviderItemIdentifier(remote.id)
        parentItemIdentifier = parent
    }
    var filename: String { remote.name }
    var contentType: UTType { remote.type == 1 ? .folder : (UTType(filenameExtension: (remote.name as NSString).pathExtension) ?? .data) }
    var capabilities: NSFileProviderItemCapabilities {
        // macOS 12.3 uses this capability; newer systems use contentPolicy.
        var result: NSFileProviderItemCapabilities = [.allowsReading, .allowsEvicting]
        if remote.type == 1 { result.insert(.allowsContentEnumerating) }
        if writable {
            result.formUnion([.allowsWriting, .allowsRenaming, .allowsReparenting, .allowsDeleting])
            if remote.type == 1 { result.insert(.allowsAddingSubItems) }
        }
        if itemIdentifier == .rootContainer { result.subtract([.allowsRenaming, .allowsReparenting, .allowsDeleting]) }
        return result
    }
    var documentSize: NSNumber? { NSNumber(value: remote.size) }
    @available(macOS 13.0, *)
    var contentPolicy: NSFileProviderContentPolicy {
        itemIdentifier == .rootContainer ? .downloadLazily : .inherited
    }
    var creationDate: Date? { ISO8601DateFormatter().date(from: remote.created_at) }
    var contentModificationDate: Date? { ISO8601DateFormatter().date(from: remote.updated_at) }
    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data((remote.primary_entity ?? "").utf8), metadataVersion: Data("\(remote.path)\n\(remote.updated_at)\n\(remote.size)".utf8))
    }
}
