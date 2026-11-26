import Foundation

public extension ThreadInfo {
    /// Folds one thread lifecycle event into the thread rows straight away, so a new
    /// thread can be opened and a finished one stops ticking before `GET /v1/threads`
    /// answers (or if it fails). Returns false when the event changes nothing.
    @discardableResult
    static func upsert(_ threads: inout [ThreadInfo], with event: Event) -> Bool {
        guard let threadId = event.threadId, !threadId.isEmpty else { return false }
        let index = threads.firstIndex { $0.id == threadId }
        let existing = index.map { threads[$0] }
        let updated: ThreadInfo
        switch event.body {
        case .threadStarted(let started):
            guard existing == nil else { return false }
            updated = ThreadInfo(id: threadId, conversationId: event.conversationId, title: started.title,
                                 deviceId: started.deviceId, model: started.model, status: "running",
                                 reason: started.reason, summary: nil, created: event.ts, finished: nil)
        case .threadWaiting(let waiting):
            guard let existing, !existing.isTerminal else { return false }
            updated = existing.with(status: "waiting_device", deviceId: waiting.deviceId)
        case .threadHandedOver(let handover):
            guard let existing, !existing.isTerminal else { return false }
            updated = existing.with(status: "running", deviceId: handover.deviceId)
        case .threadFinished(let status, let summary):
            guard let existing else { return false }
            updated = existing.with(status: status, summary: summary.isEmpty ? existing.summary : summary,
                                    finished: existing.finished ?? event.ts)
        default:
            return false
        }
        if let index { threads[index] = updated } else { threads.append(updated) }
        return true
    }

    /// Replaces one chat's rows with a fetched list. A finished thread stays finished:
    /// a list fetched before the thread ended cannot make it look live again. Rows the
    /// fetch does not include yet (a thread started after the request) are kept.
    static func merge(_ threads: [ThreadInfo], fetched: [ThreadInfo], conversationId: String) -> [ThreadInfo] {
        let local = Dictionary(threads.filter { $0.conversationId == conversationId }.map { ($0.id, $0) },
                               uniquingKeysWith: { first, _ in first })
        var merged = fetched.map { row -> ThreadInfo in
            if let known = local[row.id], known.isTerminal, !row.isTerminal { return known }
            return row
        }
        let fetchedIds = Set(fetched.map(\.id))
        merged += threads.filter { $0.conversationId == conversationId && !fetchedIds.contains($0.id) }
        return threads.filter { $0.conversationId != conversationId } + merged
    }

    private func with(status: String, deviceId: String? = nil, summary: String? = nil, finished: UInt64? = nil) -> ThreadInfo {
        ThreadInfo(id: id, conversationId: conversationId, title: title, deviceId: deviceId ?? self.deviceId,
                   model: model, status: status, reason: reason, summary: summary ?? self.summary,
                   created: created, finished: finished ?? self.finished)
    }
}
