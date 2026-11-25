import AppIntents
import Foundation

struct StopThreadIntent: LiveActivityIntent {
    static let title: LocalizedStringResource = "Stop Grouper Thread"
    // VERIFY ON MAC: Confirm IntentModes.foreground compiles in Xcode 26 AppIntents SDK (replacing deprecated openAppWhenRun = true).
    static let supportedModes: IntentModes = .foreground
    @Parameter(title: "Thread ID") var threadId: String

    init() {}
    init(threadId: String) { self.threadId = threadId }

    func perform() async throws -> some IntentResult & OpensIntent {
        var components = URLComponents()
        components.scheme = "grouper"
        components.host = "stop"
        components.queryItems = [URLQueryItem(name: "thread", value: threadId)]
        let url = components.url ?? URL(string: "grouper://stop")!
        // VERIFY ON MAC: Confirm OpenURLIntent accepts custom URL scheme "grouper://" from a LiveActivityIntent on iOS 26 (Apple docs prefer Universal Links; if rejected, stop via Keychain + HubClient in-process or App Group).
        return .result(opensIntent: OpenURLIntent(url))
    }
}
