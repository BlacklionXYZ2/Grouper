import Foundation

/// Swift-side helpers on top of the generated bindings in grouper_ffi.swift.
public extension Capability {
    init(_ name: String, _ description: String) {
        self.init(name: name, description: description)
    }
}

public extension ToolError {
    static func failed(_ message: String) -> ToolError { .Failed(message: message) }
}

/// Encode a dictionary as the JSON string a tool returns.
public func toolJSON(_ value: [String: Any]) throws -> String {
    let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    return String(decoding: data, as: UTF8.self)
}
