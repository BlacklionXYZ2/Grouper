import AppIntents
import Foundation

struct StopThreadIntent: AppIntent {
    static let title: LocalizedStringResource = "Stop Grouper Thread"
    static let openAppWhenRun = true
    @Parameter(title: "Thread ID") var threadId: String

    init() {}
    init(threadId: String) { self.threadId = threadId }

    func perform() async throws -> some IntentResult & OpensIntent {
        var components = URLComponents()
        components.scheme = "grouper"
        components.host = "stop"
        components.queryItems = [URLQueryItem(name: "thread", value: threadId)]
        return .result(opensIntent: OpenURLIntent(components.url!))
    }
}
