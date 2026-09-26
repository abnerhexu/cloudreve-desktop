import Foundation

struct DomainConfiguration: Codable {
    let server: String
    let root: String
    let domain: String
    var tokens: [String: String]
    var writable: Bool?
    var user_id: String?
    var root_id: String?

    func replacementScope(for incoming: DomainConfiguration) -> DomainConfiguration {
        // Cloudreve's reauthorize: OAuth callback omits folder/name fields.
        // The newly authenticated request must still resolve the same root ID.
        DomainConfiguration(server: incoming.server,
                            root: incoming.root.isEmpty ? root : incoming.root,
                            domain: incoming.domain, tokens: incoming.tokens,
                            writable: writable,
                            user_id: (incoming.user_id ?? "").isEmpty ? user_id : incoming.user_id)
    }

    func validateReplacement(_ replacement: DomainConfiguration) throws {
        // A path such as cloudreve://my is account-relative. Do not attach
        // existing local items to a different account or a recreated folder.
        guard domain == replacement.domain, server == replacement.server,
              root == replacement.root,
              let user_id, !user_id.isEmpty, user_id == replacement.user_id,
              let root_id, !root_id.isEmpty, root_id == replacement.root_id else {
            throw NSError(domain: "CloudreveAccountScopeMismatch", code: 1)
        }
    }
}
