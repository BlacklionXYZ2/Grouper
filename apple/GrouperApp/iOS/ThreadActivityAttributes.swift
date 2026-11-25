import ActivityKit
import Foundation

struct ThreadActivityAttributes: ActivityAttributes {
    struct ContentState: Codable, Hashable {
        var status: String
        var phase: String
        var streamedLine: String?
    }
    var threadId: String
    var title: String
    var device: String
}
