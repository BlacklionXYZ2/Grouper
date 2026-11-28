import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
@testable import GrouperAPI

final class GrouperAPILiveTests: XCTestCase {
    func testRealHubChatStreamReconnectApprovalAndFolding() async throws {
        let environment = try liveEnvironment()
        let client = try HubClient(baseURL: environment.baseURL, token: environment.adminToken)

        let hub = try await client.hubInfo()
        XCTAssertFalse(hub.hubId.isEmpty)
        _ = try await client.conversations()

        let chat = try await client.sendChat(
            text: "Swift live E2E: start the approval shell thread.",
            mode: "project"
        )
        let conversationId = chat.conversationId
        let stream = client.eventStream(conversationId: conversationId)
        let capture = LiveCapture(conversationId: conversationId)
        let collector = Task {
            do {
                for try await update in stream.updates() {
                    await capture.accept(update)
                }
            } catch {
                await capture.record(error)
            }
        }
        defer { collector.cancel() }

        try await eventually("the event stream to connect") {
            await capture.connectedCount > 0
        }
        try await eventually("a seq-0 assistant delta") {
            await capture.hasAssistantDelta
        }
        try await eventually("the device run_shell approval") {
            await capture.pendingApproval != nil
        }

        let disconnectedBefore = await capture.disconnectedCount
        XCTAssertTrue(stream.forceReconnect(), "the live socket should be active")
        try await eventually("the forced socket disconnect") {
            await capture.disconnectedCount > disconnectedBefore
        }

        let capturedApproval = await capture.pendingApproval
        let approval = try XCTUnwrap(capturedApproval)
        let decision = try await client.resolveApproval(id: approval.approvalId, approve: true)
        XCTAssertEqual(decision.approvalId, approval.approvalId)
        XCTAssertEqual(decision.approved, true)

        try await eventually("ApprovalResolved to arrive after reconnect") {
            await capture.hasResolvedApproval(approval.approvalId)
        }
        try await eventually("both the hub and device threads to finish") {
            await capture.doneThreadCount == 2
        }

        let streamError = await capture.streamError
        XCTAssertNil(streamError)
        let storedSequences = await capture.rawStoredSequences
        XCTAssertFalse(storedSequences.isEmpty)
        XCTAssertEqual(storedSequences, storedSequences.sorted(), "stored events must arrive in sequence order")
        XCTAssertEqual(storedSequences.count, Set(storedSequences).count, "reconnect must not yield duplicates")
        for (left, right) in zip(storedSequences, storedSequences.dropFirst()) {
            XCTAssertEqual(right, left + 1, "reconnect must not leave a sequence hole")
        }

        let threads = try await client.threads(conversationId: conversationId)
        XCTAssertEqual(threads.count, 2)
        XCTAssertTrue(threads.allSatisfy { $0.conversationId == conversationId })
        XCTAssertTrue(threads.allSatisfy { $0.status == "done" })
        XCTAssertTrue(threads.contains { $0.deviceId == "hub" })

        let rows = await capture.transcript(threads: threads)
        XCTAssertTrue(rows.contains { row in
            guard case .assistant = row else { return false }
            return true
        })
        let cards = rows.compactMap { row -> ThreadCard? in
            guard case .thread(let card) = row else { return nil }
            return card
        }
        XCTAssertEqual(cards.count, 2)
        XCTAssertTrue(cards.allSatisfy { $0.status == "done" })
        XCTAssertTrue(cards.allSatisfy { !$0.title.isEmpty && $0.summary != nil })
        XCTAssertTrue(cards.allSatisfy { $0.pendingApprovals == 0 })
        XCTAssertNotNil(cards.first { $0.title == "Hub stream" })
        XCTAssertNotNil(cards.first { $0.title == "Approval shell" })

        let history = try await client.events(conversationId: conversationId)
        XCTAssertEqual(history.map(\.seq), storedSequences)
        XCTAssertFalse(history.contains { $0.seq == 0 })
    }

    func testRealHubAuthenticationErrors() async throws {
        let environment = try liveEnvironment()

        let badClient = try HubClient(baseURL: environment.baseURL, token: environment.adminToken + "-wrong")
        do {
            _ = try await badClient.conversations()
            XCTFail("a bad token unexpectedly read conversations")
        } catch let error as HubClientError {
            guard case .unauthorized = error else { return XCTFail("expected 401, got \(error)") }
        }

        guard let deviceToken = environment.deviceToken else {
            throw XCTSkip("GROUPER_LIVE_DEVICE_TOKEN is needed for the device-token 403 check")
        }
        let deviceClient = try HubClient(baseURL: environment.baseURL, token: deviceToken)
        do {
            _ = try await deviceClient.sendChat(text: "device tokens cannot chat", mode: "project")
            XCTFail("a device token unexpectedly used an admin route")
        } catch let error as HubClientError {
            guard case .forbidden = error else { return XCTFail("expected 403, got \(error)") }
        }
    }

    private func liveEnvironment() throws -> LiveEnvironment {
        let values = ProcessInfo.processInfo.environment
        guard let base = values["GROUPER_LIVE_HUB"], !base.isEmpty,
              let token = values["GROUPER_LIVE_TOKEN"], !token.isEmpty else {
            throw XCTSkip("set GROUPER_LIVE_HUB and GROUPER_LIVE_TOKEN to run real-hub tests")
        }
        guard let url = URL(string: base) else {
            throw HubClientError.invalidBaseURL
        }
        return LiveEnvironment(baseURL: url, adminToken: token, deviceToken: values["GROUPER_LIVE_DEVICE_TOKEN"])
    }

    private func eventually(
        _ description: String,
        timeout: Duration = .seconds(30),
        condition: @escaping @Sendable () async -> Bool
    ) async throws {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        while clock.now < deadline {
            if await condition() { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTFail("timed out waiting for \(description)")
        throw LiveTestFailure.timeout(description)
    }
}

private struct LiveEnvironment {
    let baseURL: URL
    let adminToken: String
    let deviceToken: String?
}

private enum LiveTestFailure: Error {
    case timeout(String)
}

private actor LiveCapture {
    private var folder: TranscriptFolder
    private(set) var rawStoredSequences: [Int64] = []
    private(set) var connectedCount = 0
    private(set) var disconnectedCount = 0
    private(set) var streamError: String?
    private var liveEvents: [Event] = []

    init(conversationId: String) {
        folder = TranscriptFolder(conversationId: conversationId)
    }

    func accept(_ update: EventStreamUpdate) {
        switch update {
        case .state(.connected): connectedCount += 1
        case .state(.disconnected): disconnectedCount += 1
        case .state(.connecting): break
        case .stored(let event):
            rawStoredSequences.append(event.seq)
            folder.ingest(event)
        case .live(let event):
            liveEvents.append(event)
            folder.ingest(event)
        }
    }

    func record(_ error: Error) {
        streamError = String(describing: error)
    }

    var hasAssistantDelta: Bool {
        liveEvents.contains { event in
            guard event.seq == 0, case .assistantDelta = event.body else { return false }
            return true
        }
    }

    var pendingApproval: ApprovalRequest? {
        folder.pendingApprovals(threads: []).first?.request
    }

    func hasResolvedApproval(_ id: String) -> Bool {
        folder.storedEvents.contains { event in
            guard case .approvalResolved(let approvalId, let approved, _) = event.body else { return false }
            return approvalId == id && approved
        }
    }

    var doneThreadCount: Int {
        folder.storedEvents.filter { event in
            guard case .threadFinished(let status, _) = event.body else { return false }
            return status == "done"
        }.count
    }

    func transcript(threads: [ThreadInfo]) -> [TranscriptRow] {
        folder.transcript(threads: threads)
    }
}

private extension PendingApproval {
    var request: ApprovalRequest {
        ApprovalRequest(approvalId: approvalId, deviceId: deviceId, tool: tool, input: input, reason: reason)
    }
}
