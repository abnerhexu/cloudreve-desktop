import Foundation
import FileProvider
import Security

@main
struct ProviderHost {
    static func validateAccount(_ config: inout DomainConfiguration) throws {
        var request = try JSONSerialization.jsonObject(with: JSONEncoder().encode(config)) as! [String: Any]
        request["operation"] = "item"
        request["id"] = "root"
        let data = try Call().run(JSONSerialization.data(withJSONObject: request))
        let response = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        if let error = response["error"] as? String {
            switch error {
            case "not_authenticated": throw NSFileProviderError(.notAuthenticated)
            case "unreachable": throw NSFileProviderError(.serverUnreachable)
            case "not_found": throw NSFileProviderError(.noSuchItem)
            case "permission": throw CocoaError(.fileReadNoPermission)
            default: throw CocoaError(.coderInvalidValue)
            }
        }
        guard let file = response["data"] as? [String: Any], file["type"] as? Int == 1,
              let id = file["id"] as? String else { throw CocoaError(.fileReadCorruptFile) }
        config.root_id = id
        if let tokens = response["tokens"] as? [String: String] { config.tokens = tokens }
    }

    static func domainInfo(_ domain: NSFileProviderDomain) -> [String: Any] {
        var info: [String: Any] = ["id": domain.identifier.rawValue, "name": domain.displayName,
                                  "user_enabled": domain.userEnabled, "disconnected": domain.isDisconnected]
        do {
            let config = try Credentials.load(domain.identifier.rawValue)
            info["server"] = config.server
            info["root"] = config.root
            info["writable"] = config.writable == true
            info["credentials_accessible"] = true
            info["can_reauthorize"] = !(config.user_id ?? "").isEmpty && !(config.root_id ?? "").isEmpty
            // Accessibility alone does not establish whether the server accepts a token.
        } catch {
            info["credentials_accessible"] = false
            info["error_domain"] = (error as NSError).domain
            info["error_code"] = (error as NSError).code
        }
        return info
    }
    static func output(_ value: Any) throws {
        let data = try JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed])
        FileHandle.standardOutput.write(data)
    }
    // Configuration checks are separate from OS discovery. Missing Team ID is a
    // requirement of this shared-Keychain implementation, not proof that macOS
    // cannot discover an ad-hoc signed File Provider extension.
    static func configurationIssues() -> [String] {
        var issues: [String] = []
        let group = Bundle.main.object(forInfoDictionaryKey: "CloudreveKeychainGroup") as? String ?? ""
        if !Credentials.localKeychain && (group.isEmpty || group.contains("$(")) { issues.append("keychain_group_unconfigured") }
        let extensionURL = Bundle.main.bundleURL.appendingPathComponent("Contents/PlugIns/CloudreveFileProvider.appex")
        var teams: [String] = []
        for (label, url) in [("host", Bundle.main.bundleURL), ("extension", extensionURL)] {
            var code: SecStaticCode?
            guard SecStaticCodeCreateWithPath(url as CFURL, [], &code) == errSecSuccess, let code else {
                issues.append("\(label)_missing"); continue
            }
            if SecStaticCodeCheckValidity(code, [], nil) != errSecSuccess { issues.append("\(label)_signature_invalid") }
            var information: CFDictionary?
            guard SecCodeCopySigningInformation(code, SecCSFlags(rawValue: kSecCSSigningInformation), &information) == errSecSuccess,
                  let info = information as? [String: Any] else {
                issues.append("\(label)_signing_information_unavailable"); continue
            }
            if let team = info[kSecCodeInfoTeamIdentifier as String] as? String { teams.append(team) }
            else if !Credentials.localKeychain { issues.append("\(label)_keychain_team_missing") }
            let entitlements = info[kSecCodeInfoEntitlementsDict as String] as? [String: Any] ?? [:]
            if !Credentials.localKeychain && !(entitlements["keychain-access-groups"] as? [String] ?? []).contains(group) {
                issues.append("\(label)_keychain_entitlement_missing")
            }
            if label == "extension", entitlements["com.apple.security.app-sandbox"] as? Bool != true {
                issues.append("extension_sandbox_missing")
            }
        }
        if teams.count == 2 && teams[0] != teams[1] { issues.append("keychain_team_mismatch") }
        return issues
    }
    static func diagnostic() async -> [String: Any] {
        let issues = configurationIssues()
        var report: [String: Any] = ["available": false, "configuration_issues": issues,
                                    "reason": issues.isEmpty ? "provider_unavailable" : "configuration_incomplete",
                                    "system_discovered": false]
        do {
            let domains = try await NSFileProviderManager.domains()
            report["registered_domains"] = domains.count
            report["domains"] = domains.map {
                ["id": $0.identifier.rawValue, "user_enabled": $0.userEnabled,
                 "disconnected": $0.isDisconnected] as [String: Any]
            }
            report["system_discovered"] = true
            report["available"] = issues.isEmpty
            if issues.isEmpty { report["reason"] = "" }
        } catch {
            let error = error as NSError
            report["system_error_domain"] = error.domain
            report["system_error_code"] = error.code
            if let underlying = error.userInfo[NSUnderlyingErrorKey] as? NSError {
                report["underlying_error_domain"] = underlying.domain
                report["underlying_error_code"] = underlying.code
            }
        }
        return report
    }
    static func main() async {
        do {
            let args = CommandLine.arguments
            guard args.count >= 2 else { throw CocoaError(.coderInvalidValue) }
            if args[1] == "diagnose" || args[1] == "status" {
                try output(await diagnostic()); return
            }
            if args[1] == "probe-keychain", Credentials.localKeychain {
                let id = UUID().uuidString
                let config = DomainConfiguration(server: "https://invalid.invalid", root: "cloudreve://my", domain: id, tokens: [:], writable: false)
                try Credentials.save(config)
                defer { try? Credentials.remove(id) }
                guard try Credentials.load(id).domain == id else { throw CocoaError(.coderInvalidValue) }
                var report: [String: Any] = ["keychain_roundtrip": true]
                if args.count == 3 && args[2] == "--extension" {
                    let domain = NSFileProviderDomain(identifier: .init(id), displayName: "Cloudreve credential probe")
                    try await NSFileProviderManager.add(domain)
                    try await Task.sleep(nanoseconds: 5_000_000_000)
                    _ = try await NSFileProviderManager.remove(domain, mode: .preserveDownloadedUserData)
                    report["domain_removed"] = true
                }
                try output(report); return
            }
            // Local diagnostic only: no credentials or server configuration are
            // installed, so the extension must fail authentication without I/O.
            if args[1] == "probe-domain", Bundle.main.object(forInfoDictionaryKey: "CloudreveLocalRegistrationProbe") as? Bool == true {
                let domain = NSFileProviderDomain(identifier: .init(UUID().uuidString), displayName: "Cloudreve local loading probe")
                try await NSFileProviderManager.add(domain)
                var report: [String: Any] = ["domain_added": true, "id": domain.identifier.rawValue]
                do {
                    guard let manager = NSFileProviderManager(for: domain) else { throw NSFileProviderError(.providerNotFound) }
                    let url = try await manager.getUserVisibleURL(for: .rootContainer)
                    report["location"] = url.path
                    try await manager.signalEnumerator(for: .workingSet)
                    let waitForEnable = args.count == 3 && args[2] == "--wait-enabled"
                    if waitForEnable {
                        try output(["waiting_for_enable": true, "id": domain.identifier.rawValue, "location": url.path])
                        FileHandle.standardOutput.write(Data("\n".utf8))
                        for _ in 0..<300 {
                            if let current = try await NSFileProviderManager.domains().first(where: { $0.identifier == domain.identifier }), current.userEnabled {
                                report["user_enabled"] = true
                                break
                            }
                            try await Task.sleep(nanoseconds: 1_000_000_000)
                        }
                    }
                    try await Task.sleep(nanoseconds: 5_000_000_000)
                    if let current = try await NSFileProviderManager.domains().first(where: { $0.identifier == domain.identifier }) {
                        report["user_enabled"] = current.userEnabled
                    }
                } catch {
                    report["probe_error_domain"] = (error as NSError).domain
                    report["probe_error_code"] = (error as NSError).code
                }
                // This domain has no credentials and no user files.
                _ = try await NSFileProviderManager.remove(domain, mode: .preserveDownloadedUserData)
                report["domain_removed"] = true
                try output(report); return
            }
            guard configurationIssues().isEmpty else { throw NSFileProviderError(.notAuthenticated) }
            let domains = try await NSFileProviderManager.domains()
            switch args[1] {
            case "test-remote":
                // Local acceptance only. Never export tokens, and never mutate
                // outside an explicitly named isolated test root.
                guard Credentials.localKeychain, args.count == 3,
                      domains.contains(where: { $0.identifier.rawValue == args[2] }) else { throw CocoaError(.featureUnsupported) }
                let config = try Credentials.load(args[2])
                let prefix = "cloudreve://my/macos-fp-smoke-"
                guard config.root.hasPrefix(prefix), UUID(uuidString: String(config.root.dropFirst(prefix.count))) != nil else {
                    throw CocoaError(.fileWriteNoPermission)
                }
                let formatter = ISO8601DateFormatter()
                formatter.formatOptions.insert(.withFractionalSeconds)
                let expiryText = config.tokens["access_expires"] ?? ""
                let expiry = formatter.date(from: expiryText) ?? ISO8601DateFormatter().date(from: expiryText)
                guard let expiry, expiry.timeIntervalSinceNow > 125 else { throw NSFileProviderError(.notAuthenticated) }
                let input = try JSONSerialization.jsonObject(with: FileHandle.standardInput.readDataToEndOfFile()) as! [String: Any]
                guard let operation = input["operation"] as? String, ["item", "list", "fetch", "create", "modify", "delete"].contains(operation) else { throw CocoaError(.featureUnsupported) }
                var object = input
                // Trusted account configuration always wins over caller fields.
                object.merge(try JSONSerialization.jsonObject(with: JSONEncoder().encode(config)) as! [String: Any]) { _, trusted in trusted }
                // Verification must never rotate the extension's refresh token,
                // including when the server rejects a still-unexpired access token.
                var testTokens = config.tokens
                testTokens["refresh_token"] = ""
                object["tokens"] = testTokens
                let data = try Call().run(JSONSerialization.data(withJSONObject: object))
                var response = try JSONSerialization.jsonObject(with: data) as! [String: Any]
                response.removeValue(forKey: "tokens")
                try output(response)
            case "test-disconnect":
                guard Credentials.localKeychain, args.count == 3,
                      let domain = domains.first(where: { $0.identifier.rawValue == args[2] }),
                      let manager = NSFileProviderManager(for: domain),
                      try Credentials.load(args[2]).root.hasPrefix("cloudreve://my/macos-fp-smoke-") else { throw CocoaError(.featureUnsupported) }
                try await manager.disconnect(reason: "Isolated offline acceptance test", options: .temporary)
                try output(["disconnected": true])
            case "list":
                try output(domains.map { domainInfo($0) })
            case "resume":
                guard args.count == 3, let domain = domains.first(where: { $0.identifier.rawValue == args[2] }),
                      let manager = NSFileProviderManager(for: domain) else { throw NSFileProviderError(.noSuchItem) }
                if domain.isDisconnected { try await manager.reconnect() }
                try await manager.signalErrorResolved(NSFileProviderError(.notAuthenticated) as NSError)
                try await manager.signalErrorResolved(NSFileProviderError(.serverUnreachable) as NSError)
                try await manager.signalEnumerator(for: .workingSet)
                try output(["resumed": true])
            case "evict":
                guard args.count == 4, let domain = domains.first(where: { $0.identifier.rawValue == args[2] }),
                      let manager = NSFileProviderManager(for: domain), !args[3].isEmpty, args[3] != "root" else { throw CocoaError(.coderInvalidValue) }
                try await manager.evictItem(identifier: .init(args[3]))
                try output(["evicted": true])
            case "location", "remove":
                guard args.count == 3, let domain = domains.first(where: { $0.identifier.rawValue == args[2] }) else { throw NSFileProviderError(.noSuchItem) }
                if args[1] == "location" {
                    guard let manager = NSFileProviderManager(for: domain) else { throw NSFileProviderError(.providerNotFound) }
                    let url = try await manager.getUserVisibleURL(for: .rootContainer)
                    try output(["path": url.path])
                } else {
                    let preserved = try await NSFileProviderManager.remove(domain, mode: .preserveDownloadedUserData)
                    try Credentials.remove(domain.identifier.rawValue)
                    try output(["preserved_path": preserved?.path ?? ""])
                }
            case "register", "register-readonly", "reauthorize":
                guard args.count == 3 else { throw CocoaError(.coderInvalidValue) }
                var config = try JSONDecoder().decode(DomainConfiguration.self, from: FileHandle.standardInput.readDataToEndOfFile())
                if args[1] == "reauthorize" {
                    config = try Credentials.load(config.domain).replacementScope(for: config)
                }
                let root = URLComponents(string: config.root)
                guard UUID(uuidString: config.domain) != nil, root?.scheme == "cloudreve", root?.host == "my", root?.query == nil, root?.fragment == nil,
                      let url = URL(string: config.server), ["http", "https"].contains(url.scheme) else { throw CocoaError(.coderInvalidValue) }
                if args[1] == "register-readonly" { config.writable = false }
                if args[1] == "reauthorize" {
                    guard let domain = domains.first(where: { $0.identifier.rawValue == config.domain }),
                          let manager = NSFileProviderManager(for: domain) else { throw NSFileProviderError(.noSuchItem) }
                    let old = try Credentials.load(config.domain)
                    try validateAccount(&config)
                    try old.validateReplacement(config)
                    config.writable = old.writable
                    // Quiesce the extension before replacing its refresh-token pair.
                    // On failure, the visible disconnected domain can be resumed.
                    try await manager.disconnect(reason: "Updating account authorization", options: .temporary)
                    do { try Credentials.save(config) }
                    catch { try? await manager.reconnect(); throw error }
                    try await manager.reconnect()
                    try await manager.signalErrorResolved(NSFileProviderError(.notAuthenticated) as NSError)
                    try await manager.signalEnumerator(for: .workingSet)
                    try output(["id": config.domain]); return
                }
                if domains.contains(where: { $0.identifier.rawValue == config.domain }) {
                    let old = try Credentials.load(config.domain)
                    guard old.server == config.server && old.root == config.root else { throw CocoaError(.fileWriteFileExists) }
                    try output(["id": config.domain]); return
                }
                try validateAccount(&config)
                try Credentials.save(config)
                do {
                    try await NSFileProviderManager.add(NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(config.domain), displayName: args[2]))
                } catch {
                    // The system may have committed registration before transport
                    // failed. Keep credentials if the domain is now present.
                    if let current = try? await NSFileProviderManager.domains(), !current.contains(where: { $0.identifier.rawValue == config.domain }) { try? Credentials.remove(config.domain) }
                    throw error
                }
                try output(["id": config.domain])
            default: throw CocoaError(.coderInvalidValue)
            }
        } catch {
            let error = error as NSError
            // Return only fixed domains/codes, never server bodies or credential data.
            try? output(["error_domain": error.domain, "error_code": error.code])
            fputs("File Provider: \(error.domain) (\(error.code))\n", stderr)
            exit(1)
        }
    }
}
