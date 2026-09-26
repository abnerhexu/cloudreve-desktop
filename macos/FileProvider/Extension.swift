import Foundation
import FileProvider
import OSLog

@objc(CloudreveFileProvider)
final class CloudreveFileProvider: NSObject, NSFileProviderReplicatedExtension {
    private static let logger = Logger(subsystem: "cloudreve.desktop.finder-preview.fileprovider", category: "lifecycle")
    private let backend: Backend
    private let timer: DispatchSourceTimer
    required init(domain: NSFileProviderDomain) {
        backend = Backend(domain: domain)
        timer = DispatchSource.makeTimerSource(queue: backend.queue)
        super.init()
        Self.logger.notice("File Provider initialized")
        if Credentials.localKeychain {
            let accessible = (try? Credentials.load(domain.identifier.rawValue)) != nil
            Self.logger.notice("Local account configuration accessible: \(accessible, privacy: .public)")
        }
        timer.schedule(deadline: .now(), repeating: 30)
        timer.setEventHandler { [weak self] in
            self?.backend.manager.signalEnumerator(for: .workingSet) { _ in }
        }
        timer.resume()
    }
    func invalidate() {
        Self.logger.notice("File Provider invalidated")
        timer.cancel()
    }
    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest, completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        let call = Call()
        backend.queue.async {
            do { completionHandler(try self.backend.item(identifier, call: call), nil) }
            catch { completionHandler(nil, error) }
            call.progress.completedUnitCount = 1000
        }
        return call.progress
    }
    func fetchContents(for identifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?, request: NSFileProviderRequest, completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        let call = Call()
        backend.queue.async {
            var staging: URL?
            do {
                let file = try self.backend.manager.temporaryDirectoryURL().appendingPathComponent(UUID().uuidString)
                staging = file
                let version = requestedVersion.map { String(decoding: $0.contentVersion, as: UTF8.self) } ?? ""
                let remote: RemoteFile = try self.backend.request("fetch", ["id": identifier.rawValue, "version": version, "local_path": file.path], call: call, as: RemoteFile.self)
                let item = try self.backend.item(identifier, call: call)
                // If the remote changed during hydration, return metadata for the
                // downloaded version, never advertise newer bytes than we fetched.
                completionHandler(file, Item(remote, parent: item.parentItemIdentifier, writable: self.backend.writable), nil)
            } catch {
                if let staging { try? FileManager.default.removeItem(at: staging) }
                completionHandler(nil, nil, error)
            }
            call.progress.completedUnitCount = 1000
        }
        return call.progress
    }
    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        if containerItemIdentifier == .trashContainer { throw CocoaError(.featureUnsupported) }
        return Enumerator(backend, container: containerItemIdentifier)
    }
    private func identifier(_ id: NSFileProviderItemIdentifier) -> String { id == .rootContainer ? "root" : id.rawValue }
    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents: URL?, options: NSFileProviderCreateItemOptions, request: NSFileProviderRequest, completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let call = Call()
        backend.queue.async {
            do {
                guard self.backend.writable else { throw CocoaError(.fileWriteNoPermission) }
                // A dataless import must not become an empty remote file.
                if itemTemplate.contentType != .folder && contents == nil { throw CocoaError(.featureUnsupported) }
                let remote: RemoteFile = try self.backend.request("create", [
                    "parent": self.identifier(itemTemplate.parentItemIdentifier), "name": itemTemplate.filename,
                    "directory": itemTemplate.contentType == .folder,
                    "request_id": itemTemplate.itemIdentifier.rawValue, "local_path": contents?.path ?? ""
                ], call: call, as: RemoteFile.self)
                let result = try self.backend.item(NSFileProviderItemIdentifier(remote.id), call: call)
                let remaining = fields.subtracting([.filename, .parentItemIdentifier, .contents])
                completionHandler(result, remaining, false, nil)
                self.backend.manager.signalEnumerator(for: .workingSet) { _ in }
            } catch { completionHandler(nil, fields, false, error) }
            call.progress.completedUnitCount = 1000
        }
        return call.progress
    }
    func modifyItem(_ item: NSFileProviderItem, baseVersion: NSFileProviderItemVersion, changedFields: NSFileProviderItemFields, contents: URL?, options: NSFileProviderModifyItemOptions, request: NSFileProviderRequest, completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let call = Call()
        backend.queue.async {
            do {
                guard self.backend.writable else { throw CocoaError(.fileWriteNoPermission) }
                if changedFields.contains(.contents) && contents == nil { throw CocoaError(.featureUnsupported) }
                let remote: RemoteFile = try self.backend.request("modify", [
                    "id": item.itemIdentifier.rawValue,
                    "version": String(decoding: baseVersion.contentVersion, as: UTF8.self),
                    "metadata_version": String(decoding: baseVersion.metadataVersion, as: UTF8.self),
                    "parent": changedFields.contains(.parentItemIdentifier) ? self.identifier(item.parentItemIdentifier) : "",
                    "name": changedFields.contains(.filename) ? item.filename : "",
                    "local_path": changedFields.contains(.contents) ? (contents?.path ?? "") : ""
                ], call: call, as: RemoteFile.self)
                let result = try self.backend.item(NSFileProviderItemIdentifier(remote.id), call: call)
                completionHandler(result, changedFields.subtracting([.filename, .parentItemIdentifier, .contents]), false, nil)
                self.backend.manager.signalEnumerator(for: .workingSet) { _ in }
            } catch { completionHandler(nil, changedFields, false, error) }
            call.progress.completedUnitCount = 1000
        }
        return call.progress
    }
    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion: NSFileProviderItemVersion, options: NSFileProviderDeleteItemOptions, request: NSFileProviderRequest, completionHandler: @escaping (Error?) -> Void) -> Progress {
        if identifier == .rootContainer {
            completionHandler(CocoaError(.fileWriteNoPermission))
            return Progress(totalUnitCount: 0)
        }
        let call = Call()
        backend.queue.async {
            do {
                guard self.backend.writable else { throw CocoaError(.fileWriteNoPermission) }
                let _: RemoteFile? = try self.backend.request("delete", [
                    "id": identifier.rawValue,
                    "version": String(decoding: baseVersion.contentVersion, as: UTF8.self),
                    "metadata_version": String(decoding: baseVersion.metadataVersion, as: UTF8.self)
                ], call: call, as: RemoteFile?.self)
                completionHandler(nil)
                self.backend.manager.signalEnumerator(for: .workingSet) { _ in }
            } catch {
                if (error as NSError).domain == NSFileProviderErrorDomain && (error as NSError).code == NSFileProviderError.cannotSynchronize.rawValue,
                   let current = try? self.backend.item(identifier, call: call) {
                    completionHandler(NSError.fileProviderErrorForRejectedDeletion(of: current))
                } else { completionHandler(error) }
            }
            call.progress.completedUnitCount = 1000
        }
        return call.progress
    }
}
