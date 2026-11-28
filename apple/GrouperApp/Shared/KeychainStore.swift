import Foundation
import GrouperAPI
import Security

enum GrouperSecret: String, Sendable {
    case admin = "grouper.admin"
    case device = "grouper.device"
    /// The hub's id for this Mac's device (not secret, kept beside its token).
    case deviceId = "grouper.device.id"
    /// This app install's random id (`InstallID`), not secret. Kept across sign-outs; only
    /// removing the app should lose it (and the Keychain may outlive even that).
    case installId = "grouper.install.id"
    /// The relay sign-in token for the hub address in `AppModel.relayHubKey` (docs/relay-design.md).
    case relay = "grouper.relay"
}

/// This app install's `install_id` (docs/accounts-design.md, "Contract"): made on first use,
/// kept in the Keychain across sign-outs, sent with Google sign-in and the Mac's node
/// registration so the hub reuses this install's devices instead of adding new ones.
@MainActor
enum AppInstall {
    private static var cached: String?

    /// The id, created and saved on first use. Nil only when the Keychain can't save it; the
    /// app then signs in without one (the hub makes a new device, as before install ids).
    static var id: String? {
        if let cached { return cached }
        if let saved = KeychainStore.read(.installId), InstallID.isValid(saved) {
            cached = saved
            return saved
        }
        let made = InstallID.generate()
        do {
            try KeychainStore.save(made, as: .installId)
        } catch {
            return nil
        }
        cached = made
        return made
    }
}

enum KeychainStore {
    private static let account = "token"

    static func read(_ secret: GrouperSecret) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: secret.rawValue,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func save(_ value: String, as secret: GrouperSecret) throws {
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: secret.rawValue,
            kSecAttrAccount as String: account,
        ]
        let data = Data(value.utf8)
        let status = SecItemUpdate(base as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if status == errSecItemNotFound {
            var create = base
            create[kSecValueData as String] = data
            // VERIFY ON MAC: Confirm SecItemAdd with kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            // behaves as expected on unsigned/non-sandboxed macOS 26 (legacy vs Data Protection Keychain).
            create[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            let created = SecItemAdd(create as CFDictionary, nil)
            guard created == errSecSuccess else { throw KeychainError.status(created) }
        } else if status != errSecSuccess {
            throw KeychainError.status(status)
        }
    }

    static func delete(_ secret: GrouperSecret) {
        _ = SecItemDelete([
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: secret.rawValue,
            kSecAttrAccount as String: account,
        ] as CFDictionary)
    }
}

enum KeychainError: Error, LocalizedError, Sendable {
    case status(OSStatus)

    var errorDescription: String? {
        switch self {
        case .status(let code):
            #if os(macOS)
            if let message = SecCopyErrorMessageString(code, nil) as String? {
                return "Keychain error: \(message) (\(code))"
            }
            #endif
            return "Keychain error (OSStatus \(code))."
        }
    }
}
