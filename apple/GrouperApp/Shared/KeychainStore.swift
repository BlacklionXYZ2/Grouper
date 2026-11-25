import Foundation
import Security

enum GrouperSecret: String {
    case admin = "grouper.admin"
    case device = "grouper.device"
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
            create[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            let created = SecItemAdd(create as CFDictionary, nil)
            guard created == errSecSuccess else { throw KeychainError.status(created) }
        } else if status != errSecSuccess {
            throw KeychainError.status(status)
        }
    }

    static func delete(_ secret: GrouperSecret) {
        SecItemDelete([
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: secret.rawValue,
            kSecAttrAccount as String: account,
        ] as CFDictionary)
    }
}

enum KeychainError: Error { case status(OSStatus) }
