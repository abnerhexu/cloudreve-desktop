import Foundation
import FileProvider

@main
struct ItemTests {
    static func main() throws {
        let file = RemoteFile(id: "stable-id", name: "文件.txt", path: "cloudreve://my/文件.txt", type: 0, size: 42, primary_entity: "content-v1", updated_at: "2026-09-21T00:00:00Z", created_at: "2026-09-20T00:00:00Z")
        let item = Item(file, parent: .rootContainer)
        assert(item.itemIdentifier.rawValue == "stable-id")
        assert(item.documentSize == 42)
        assert(item.itemVersion.contentVersion == Data("content-v1".utf8))
        assert(item.creationDate != nil)
        assert(item.capabilities.contains(.allowsReading))
        assert(item.capabilities.contains(.allowsEvicting))
        assert(!item.capabilities.contains(.allowsWriting))
        let writable = Item(file, parent: .rootContainer, writable: true)
        assert(writable.capabilities.contains(.allowsWriting))
        assert(writable.capabilities.contains(.allowsRenaming))
        let root = Item(file, id: .rootContainer, parent: .rootContainer, writable: true)
        assert(!root.capabilities.contains(.allowsDeleting))
        if #available(macOS 13.0, *) {
            assert(root.contentPolicy == .downloadLazily)
            assert(item.contentPolicy == .inherited)
        }
        let renamed = RemoteFile(id: file.id, name: "renamed.txt", path: "cloudreve://my/renamed.txt", type: 0, size: 42, primary_entity: file.primary_entity, updated_at: file.updated_at, created_at: file.created_at)
        let renamedItem = Item(renamed, parent: .rootContainer)
        assert(renamedItem.itemIdentifier == item.itemIdentifier)
        assert(renamedItem.itemVersion.contentVersion == item.itemVersion.contentVersion)
        assert(renamedItem.itemVersion.metadataVersion != item.itemVersion.metadataVersion)
        print("PASS File Provider identity, versions, dates and read/write capabilities")
        let original = DomainConfiguration(server: "https://test.invalid", root: "cloudreve://my/test", domain: "domain", tokens: [:], writable: true, user_id: "user-a", root_id: "folder-a")
        var replacement = original
        replacement.tokens = ["access_token": "new-test-token"]
        try original.validateReplacement(replacement)
        let callback = DomainConfiguration(server: original.server, root: "", domain: original.domain, tokens: replacement.tokens, writable: true, user_id: "")
        var scoped = original.replacementScope(for: callback)
        assert(scoped.root == original.root && scoped.tokens == replacement.tokens)
        assert(scoped.root_id == nil) // Must be resolved using the new token, never inherited.
        scoped.root_id = original.root_id
        try original.validateReplacement(scoped)
        for change in ["user", "folder", "legacy"] {
            var wrong = replacement
            if change == "user" { wrong.user_id = "user-b" }
            if change == "folder" { wrong.root_id = "folder-b" }
            if change == "legacy" { wrong.user_id = nil }
            do { try original.validateReplacement(wrong); fatalError("Accepted a different account/root") }
            catch { assert((error as NSError).domain == "CloudreveAccountScopeMismatch") }
        }
        print("PASS reauthorization rejects account/root changes and unknown legacy identity")
    }
}
