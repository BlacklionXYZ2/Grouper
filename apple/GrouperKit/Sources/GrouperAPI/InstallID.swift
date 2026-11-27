import Foundation

/// A random id for one app install (docs/accounts-design.md, "Contract"). The app keeps it in
/// the Keychain across sign-outs and sends it as `install_id` with `POST /v1/auth/google` and
/// `POST /v1/devices`, so signing in again from the same install reuses that install's device
/// on the hub instead of adding one. The Mac's sign-in and its node send the same id: the hub
/// keeps a separate credential for each (an admin token and a device token) but lists them as
/// one device. It is not a secret and never shown to the person.
public enum InstallID {
    /// The characters the hub accepts in an `install_id`: `[A-Za-z0-9_-]`.
    public static let alphabet = Array("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_")
    /// The length of a new id.
    public static let length = 32
    /// The hub's limits on an `install_id`'s length.
    public static let allowedLengths = 16...64

    /// A new random, URL-safe id of `length` characters.
    public static func generate() -> String {
        var generator = SystemRandomNumberGenerator()
        return String((0..<length).map { _ in alphabet[Int(generator.next(upperBound: UInt64(alphabet.count)))] })
    }

    /// Whether the hub would accept `value` as an `install_id`.
    public static func isValid(_ value: String) -> Bool {
        allowedLengths.contains(value.count) && value.unicodeScalars.allSatisfy { scalar in
            scalar.isASCII && (CharacterSet.alphanumerics.contains(scalar) || scalar == "-" || scalar == "_")
        }
    }

    /// `value` when the hub would accept it, else nil (so a bad id is left out, not refused).
    static func sendable(_ value: String?) -> String? {
        guard let value, isValid(value) else { return nil }
        return value
    }
}
