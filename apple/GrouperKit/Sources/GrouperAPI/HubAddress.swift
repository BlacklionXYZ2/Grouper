import Foundation

/// Reasons a hub address from Settings is rejected.
public enum HubAddressError: Error, Equatable, Sendable, LocalizedError {
    case empty
    case invalidURL
    case unsupportedScheme
    case insecureRemoteHost

    public var errorDescription: String? {
        switch self {
        case .empty:
            return "Enter a hub address."
        case .invalidURL:
            return "Enter a valid hub address, like https://grouper.example."
        case .unsupportedScheme:
            return "Hub addresses must start with https:// (or http:// for localhost or a local network address)."
        case .insecureRemoteHost:
            return "Use https:// for remote hubs. Plain http:// is only allowed for localhost and local network addresses."
        }
    }
}

/// Built-in default hub address, resolution order, and validation (docs/local-first-design.md).
public enum HubAddress: Sendable {
    /// The hub address shipped with the apps.
    public static let defaultAddress = "https://grouper.clockmillbarn.uk"

    /// The `UserDefaults` key where a Settings override is stored.
    public static let overrideKey = "grouper.hubURL"

    /// Resolves the hub address to connect to: a valid saved override from Settings, then the
    /// built-in default. An empty or invalid override falls back to `defaultAddress`.
    public static func resolve(savedOverride: String?) -> String {
        guard let savedOverride, let valid = try? validate(savedOverride) else {
            return defaultAddress
        }
        return valid
    }

    /// Validates and normalises a hub address: trims whitespace, lowercases scheme and host,
    /// strips default ports and trailing slashes, and requires HTTPS except for localhost and
    /// local network (LAN) addresses.
    public static func validate(_ raw: String) throws -> String {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { throw HubAddressError.empty }
        guard !trimmed.unicodeScalars.contains(where: { CharacterSet.whitespacesAndNewlines.contains($0) || CharacterSet.controlCharacters.contains($0) }) else {
            throw HubAddressError.invalidURL
        }
        guard trimmed.contains("://") else {
            throw HubAddressError.unsupportedScheme
        }
        guard var components = URLComponents(string: trimmed) else {
            throw HubAddressError.invalidURL
        }
        guard let scheme = components.scheme?.lowercased(), !scheme.isEmpty else {
            throw HubAddressError.unsupportedScheme
        }
        guard scheme == "https" || scheme == "http" else {
            throw HubAddressError.unsupportedScheme
        }
        guard components.user == nil, components.password == nil,
              components.query == nil, components.fragment == nil else {
            throw HubAddressError.invalidURL
        }
        if let port = components.port, !(1...65535).contains(port) {
            throw HubAddressError.invalidURL
        }
        guard let rawHost = components.host, !rawHost.isEmpty else {
            throw HubAddressError.invalidURL
        }
        let loweredHost = rawHost.lowercased()
        if scheme == "http" {
            guard isLocalOrLANHost(loweredHost) else {
                throw HubAddressError.insecureRemoteHost
            }
        }

        components.scheme = scheme
        // On Darwin URLComponents, assigning `host` with a bare IPv6 literal can percent-encode
        // colons instead of keeping `[...]`; `percentEncodedHost` keeps brackets on all platforms.
        if loweredHost.contains(":") {
            let encoded = components.percentEncodedHost ?? (loweredHost.hasPrefix("[") ? loweredHost : "[\(loweredHost)]")
            components.percentEncodedHost = encoded.lowercased()
        } else {
            components.host = loweredHost
        }
        if (scheme == "https" && components.port == 443) || (scheme == "http" && components.port == 80) {
            components.port = nil
        }
        var path = components.path
        while path.hasSuffix("/") {
            path.removeLast()
        }
        components.path = path

        guard let url = components.url, url.host != nil else {
            throw HubAddressError.invalidURL
        }
        var result = url.absoluteString
        while result.hasSuffix("/") {
            result.removeLast()
        }
        return result
    }

    /// Whether `address` resolves to the built-in default (ignoring case, spaces and trailing slashes).
    public static func isDefault(_ address: String) -> Bool {
        guard let normalized = try? validate(address) else { return false }
        return normalized == defaultAddress
    }

    /// Short display label for messages: host plus non-default port, e.g. `grouper.clockmillbarn.uk`
    /// or `localhost:47600`.
    public static func displayHost(_ address: String) -> String {
        let trimmed = address.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let components = URLComponents(string: trimmed),
              let rawHost = components.host, !rawHost.isEmpty else {
            return trimmed
        }
        let host = rawHost.lowercased()
        let formattedHost = (host.contains(":") && !host.hasPrefix("[")) ? "[\(host)]" : host
        let scheme = components.scheme?.lowercased()
        if let port = components.port,
           !((scheme == "https" && port == 443) || (scheme == "http" && port == 80)) {
            return "\(formattedHost):\(port)"
        }
        return formattedHost
    }

    /// Whether `host` is localhost or a local network (LAN) address where plain HTTP is allowed:
    /// `localhost`, `*.localhost`, `*.local` (mDNS), IPv4 loopback / RFC 1918 / link-local,
    /// and IPv6 loopback / ULA (`fc00::/7`) / link-local (`fe80::/10`) / IPv4-mapped local.
    public static func isLocalOrLANHost(_ host: String) -> Bool {
        var value = host.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if value.hasPrefix("[") && value.hasSuffix("]") {
            value = String(value.dropFirst().dropLast())
        }
        if value.contains(":"), let zoneIndex = value.firstIndex(of: "%") {
            let zone = value[value.index(after: zoneIndex)...]
            guard !zone.isEmpty else { return false }
            value = String(value[..<zoneIndex])
        }
        guard !value.isEmpty else { return false }

        if value == "localhost" {
            return true
        }
        if value.hasSuffix(".localhost"), value.count > ".localhost".count,
           !value.hasPrefix("."), !value.contains("..") {
            return true
        }
        if value.hasSuffix(".local"), value.count > ".local".count,
           !value.hasPrefix("."), !value.contains("..") {
            return true
        }
        if let ipv4 = parseIPv4(value) {
            return isLocalOrLANIPv4(ipv4)
        }
        if value.contains(":"), let ipv6 = parseIPv6(value) {
            return isLocalOrLANIPv6(ipv6)
        }
        return false
    }

    // MARK: - IP parsing

    private static func parseIPv4(_ text: String) -> (UInt8, UInt8, UInt8, UInt8)? {
        let parts = text.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else { return nil }
        var octets: [UInt8] = []
        octets.reserveCapacity(4)
        for part in parts {
            guard !part.isEmpty, part.count <= 3 else { return nil }
            guard part.unicodeScalars.allSatisfy({ $0.value >= 48 && $0.value <= 57 }) else { return nil }
            // Reject leading zeros (e.g. 0177) to avoid octal ambiguity.
            if part.count > 1 && part.first == "0" { return nil }
            guard let value = UInt8(part) else { return nil }
            octets.append(value)
        }
        return (octets[0], octets[1], octets[2], octets[3])
    }

    private static func isLocalOrLANIPv4(_ ip: (UInt8, UInt8, UInt8, UInt8)) -> Bool {
        let (a, b, _, _) = ip
        if a == 127 { return true }                          // 127.0.0.0/8 loopback
        if a == 10 { return true }                           // 10.0.0.0/8 RFC 1918
        if a == 172 && (16...31).contains(b) { return true } // 172.16.0.0/12 RFC 1918
        if a == 192 && b == 168 { return true }              // 192.168.0.0/16 RFC 1918
        if a == 169 && b == 254 { return true }              // 169.254.0.0/16 link-local
        return false
    }

    private static func parseIPv6(_ text: String) -> [UInt16]? {
        // At most one "::" compression marker.
        let halves = text.components(separatedBy: "::")
        guard halves.count == 1 || halves.count == 2 else { return nil }

        func parseGroupList(_ section: String, allowTrailingIPv4: Bool) -> [UInt16]? {
            if section.isEmpty { return [] }
            let tokens = section.split(separator: ":", omittingEmptySubsequences: false)
            var out: [UInt16] = []
            for (index, token) in tokens.enumerated() {
                let s = String(token)
                if allowTrailingIPv4 && index == tokens.count - 1 && s.contains(".") {
                    guard let (a, b, c, d) = parseIPv4(s) else { return nil }
                    out.append((UInt16(a) << 8) | UInt16(b))
                    out.append((UInt16(c) << 8) | UInt16(d))
                } else {
                    guard (1...4).contains(s.count),
                          s.unicodeScalars.allSatisfy({ isASCIIHex($0) }),
                          let value = UInt16(s, radix: 16) else { return nil }
                    out.append(value)
                }
            }
            return out
        }

        if halves.count == 1 {
            guard let groups = parseGroupList(halves[0], allowTrailingIPv4: true),
                  groups.count == 8 else { return nil }
            return groups
        } else {
            guard let left = parseGroupList(halves[0], allowTrailingIPv4: false),
                  let right = parseGroupList(halves[1], allowTrailingIPv4: true) else { return nil }
            let missing = 8 - left.count - right.count
            guard missing >= 1 else { return nil }
            return left + Array(repeating: 0, count: missing) + right
        }
    }

    private static func isLocalOrLANIPv6(_ segs: [UInt16]) -> Bool {
        guard segs.count == 8 else { return false }
        // Loopback ::1
        if segs[0] == 0 && segs[1] == 0 && segs[2] == 0 && segs[3] == 0
            && segs[4] == 0 && segs[5] == 0 && segs[6] == 0 && segs[7] == 1 {
            return true
        }
        // Unique Local Address fc00::/7 (fc00:: - fdff::)
        if (segs[0] & 0xfe00) == 0xfc00 { return true }
        // Link-local fe80::/10
        if (segs[0] & 0xffc0) == 0xfe80 { return true }
        // IPv4-mapped IPv6 ::ffff:a.b.c.d
        if segs[0] == 0 && segs[1] == 0 && segs[2] == 0 && segs[3] == 0
            && segs[4] == 0 && segs[5] == 0xffff {
            let ipv4 = (
                UInt8(segs[6] >> 8),
                UInt8(segs[6] & 0xff),
                UInt8(segs[7] >> 8),
                UInt8(segs[7] & 0xff)
            )
            return isLocalOrLANIPv4(ipv4)
        }
        return false
    }

    private static func isASCIIHex(_ scalar: Unicode.Scalar) -> Bool {
        let v = scalar.value
        return (v >= 48 && v <= 57) || (v >= 65 && v <= 70) || (v >= 97 && v <= 102)
    }
}
