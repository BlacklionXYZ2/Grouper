import Foundation
import XCTest
@testable import GrouperAPI

final class ApprovalTests: XCTestCase {
    private func event(_ json: String) throws -> Event {
        try JSONDecoder.grouper.decode(Event.self, from: Data(json.utf8))
    }

    private func requested(_ id: String, seq: Int64, ts: UInt64, conversation: String = "c1", thread: String = "t1", reason: String = "run_shell is set to Manual") throws -> Event {
        try event(#"{"seq":\#(seq),"conversation_id":"\#(conversation)","thread_id":"\#(thread)","ts":\#(ts),"kind":"approval_requested","approval_id":"\#(id)","device_id":"mac","tool":"run_shell","input":{"command":"ls"},"reason":"\#(reason)"}"#)
    }

    private func resolved(_ id: String, seq: Int64, approved: Bool = true) throws -> Event {
        try event(#"{"seq":\#(seq),"conversation_id":"c1","thread_id":"t1","ts":200,"kind":"approval_resolved","approval_id":"\#(id)","approved":\#(approved)}"#)
    }

    private func approval(_ id: String, ts: UInt64, conversation: String = "c1") -> PendingApproval {
        PendingApproval(approvalId: id, conversationId: conversation, threadId: "t1", deviceId: "mac", tool: "run_shell",
                        input: .object([:]), reason: "", seq: 0, ts: ts)
    }

    func testQueueIsOldestFirstWithIdTieBreakAndStopsAtTheEnds() {
        let queue = ApprovalQueue.ordered([approval("b", ts: 10), approval("c", ts: 5), approval("a", ts: 10)])
        XCTAssertEqual(queue.map(\.id), ["c", "a", "b"])
        XCTAssertEqual(ApprovalQueue.displayed(selected: nil, in: queue)?.id, "c")
        XCTAssertEqual(ApprovalQueue.displayed(selected: "gone", in: queue)?.id, "c")
        XCTAssertEqual(ApprovalQueue.displayed(selected: "a", in: queue)?.id, "a")
        XCTAssertNil(ApprovalQueue.neighbour(of: "c", step: -1, in: queue), "Previous is disabled at the start")
        XCTAssertEqual(ApprovalQueue.neighbour(of: "c", step: 1, in: queue), "a")
        XCTAssertEqual(ApprovalQueue.neighbour(of: "a", step: 1, in: queue), "b")
        XCTAssertNil(ApprovalQueue.neighbour(of: "b", step: 1, in: queue), "Next is disabled at the end, no wrap")
        XCTAssertNil(ApprovalQueue.neighbour(of: nil, step: 1, in: []))
    }

    func testSuccessorAdvancesToTheNextThenFallsBack() {
        let order = ["a", "b", "c"]
        XCTAssertEqual(ApprovalQueue.successor(of: "b", previousOrder: order, remaining: [approval("a", ts: 1), approval("c", ts: 3)]), "c")
        XCTAssertEqual(ApprovalQueue.successor(of: "c", previousOrder: order, remaining: [approval("a", ts: 1), approval("b", ts: 2)]), "b")
        XCTAssertNil(ApprovalQueue.successor(of: "a", previousOrder: ["a"], remaining: []))
    }

    func testRiskRuleReadsTheHubReason() {
        XCTAssertFalse(ApprovalQueue.isRiskRule(reason: "run_shell is set to Manual"))
        XCTAssertFalse(ApprovalQueue.isRiskRule(reason: ""), "no reason: treat as Manual, don't claim a rule fired")
        XCTAssertTrue(ApprovalQueue.isRiskRule(reason: "Deletes files outside the workspace"))
        XCTAssertTrue(ApprovalQueue.isRiskRule(reason: "run_shell is set to Manual, but also rm -rf"))
    }

    func testBookFoldsEventsAndRemembersWhyRequestsLeft() throws {
        var book = ApprovalBook()
        let first = book.apply(try requested("a1", seq: 1, ts: 100))
        let added = try XCTUnwrap(book.pending["a1"])
        XCTAssertEqual(first, .added(added))
        let repeated = book.apply(try requested("a1", seq: 1, ts: 100))
        XCTAssertEqual(repeated, .none, "a repeat is not a new request")
        _ = book.apply(try requested("a2", seq: 2, ts: 101))
        XCTAssertEqual(book.ordered.map(\.id), ["a1", "a2"])

        let change1 = book.apply(try resolved("a1", seq: 3))
        XCTAssertEqual(change1, .removed(["a1"]))
        XCTAssertEqual(book.outcome(of: "a1"), .answeredElsewhere)
        let change2 = book.apply(try requested("a1", seq: 1, ts: 100))
        XCTAssertEqual(change2, .none, "a settled request can't come back")

        let finished = try event(#"{"seq":4,"conversation_id":"c1","thread_id":"t1","ts":300,"kind":"thread_finished","status":"stopped","summary":""}"#)
        let change3 = book.apply(finished)
        XCTAssertEqual(change3, .removed(["a2"]))
        XCTAssertEqual(book.outcome(of: "a2"), .threadEnded)
        XCTAssertTrue(book.pending.isEmpty)
    }

    func testResolutionReasonSaysTimedOutOrNoLongerWaiting() throws {
        var book = ApprovalBook()
        for (i, id) in ["a_1", "a_2", "a_3"].enumerated() { _ = book.apply(try requested(id, seq: Int64(i + 1), ts: 100)) }
        let base = #""conversation_id":"c1","thread_id":"t1","ts":200,"kind":"approval_resolved","approved":false"#
        _ = book.apply(try event(#"{"seq":4,\#(base),"approval_id":"a_1","reason":"timeout"}"#))
        _ = book.apply(try event(#"{"seq":5,\#(base),"approval_id":"a_2","reason":"timeout_stop"}"#))
        _ = book.apply(try event(#"{"seq":6,\#(base),"approval_id":"a_3","reason":"dropped"}"#))
        XCTAssertEqual(book.outcome(of: "a_1"), .timedOut(threadStopped: false))
        XCTAssertEqual(book.outcome(of: "a_2"), .timedOut(threadStopped: true))
        XCTAssertEqual(book.outcome(of: "a_3"), .noLongerWaiting)
        XCTAssertTrue(book.pending.isEmpty)
        // Older hubs send no reason: the event still decodes.
        let old = try event(#"{"seq":7,\#(base),"approval_id":"a_4"}"#)
        guard case .approvalResolved(_, _, let reason) = old.body else { return XCTFail("\(old.body)") }
        XCTAssertNil(reason)
    }

    func testOwnAnswerIsNotMistakenForAnotherDevice() throws {
        var book = ApprovalBook()
        _ = book.apply(try requested("a1", seq: 1, ts: 100))
        book.beginAnswer("a1")
        // The socket's approval_resolved can beat the HTTP response.
        let change4 = book.apply(try resolved("a1", seq: 2, approved: false))
        XCTAssertEqual(change4, .removed(["a1"]))
        XCTAssertEqual(book.outcome(of: "a1"), .answeredHere(approved: false))
        let change5 = book.settle("a1", as: .answeredHere(approved: false))
        XCTAssertEqual(change5, .none)
        XCTAssertEqual(book.outcome(of: "a1"), .answeredHere(approved: false))

        _ = book.apply(try requested("a2", seq: 3, ts: 101))
        let change6 = book.settle("a2", as: .answeredElsewhere)
        XCTAssertEqual(change6, .removed(["a2"]), "a 404 settles it as answered elsewhere")
        XCTAssertEqual(book.outcome(of: "a2"), .answeredElsewhere)
    }

    func testFetchIsAuthoritativeButKeepsArrivalsDuringTheFetch() throws {
        var book = ApprovalBook()
        _ = book.apply(try requested("old", seq: 1, ts: 100))
        let mark = book.fetchMark()
        _ = book.apply(try requested("new", seq: 2, ts: 102))
        _ = book.apply(try requested("gone", seq: 3, ts: 103))
        _ = book.apply(try resolved("gone", seq: 4))
        let fetched = [
            HubApproval(approvalId: "old", deviceId: "mac", threadId: "t1", conversationId: "c1", tool: "run_shell", input: .null,
                        reason: "r", requestedAt: 99, timeoutSecs: 300, timeoutAt: 399, timeoutAction: .stop).pending,
            HubApproval(approvalId: "gone", deviceId: "mac", threadId: "t1", conversationId: "c1", tool: "run_shell", input: .null,
                        reason: "r", requestedAt: 103, timeoutSecs: nil, timeoutAt: nil, timeoutAction: nil).pending,
            HubApproval(approvalId: "elsewhere", deviceId: "pc", threadId: "t9", conversationId: "c2", tool: "write_file", input: .null,
                        reason: "r", requestedAt: 50, timeoutSecs: nil, timeoutAt: nil, timeoutAction: nil).pending,
        ]
        let change7 = book.finishFetch(fetched, since: mark)
        XCTAssertEqual(change7, .none)
        XCTAssertTrue(book.loaded)
        XCTAssertEqual(book.ordered.map(\.id), ["elsewhere", "old", "new"], "the fetch missed 'new', which arrived after it began")
        XCTAssertEqual(book.pending["old"]?.timeoutAt, 399)
        XCTAssertEqual(book.pending["old"]?.seq, 1, "keeps the event's seq")
        XCTAssertEqual(book.ordered(in: "c2").map(\.id), ["elsewhere"])

        // A later fetch without them: they left while we weren't looking.
        let laterMark = book.fetchMark()
        let change8 = book.finishFetch([], since: laterMark)
        XCTAssertEqual(change8, .removed(["elsewhere", "new", "old"]))
        XCTAssertEqual(book.outcome(of: "old"), .noLongerWaiting)
    }

    func testHubApprovalDecodes() throws {
        let json = #"[{"approval_id":"a1","device_id":"mac","thread_id":"t1","conversation_id":"c1","tool":"run_shell","input":{"command":"ls"},"reason":"run_shell is set to Manual","requested_at":1700000000,"timeout_secs":90,"timeout_at":1700000090,"timeout_action":"stop"},{"approval_id":"a2","device_id":"mac","thread_id":"t1","conversation_id":"c1","tool":"run_shell","input":null,"reason":"","requested_at":1700000001}]"#
        let decoded = try JSONDecoder.grouper.decode([HubApproval].self, from: Data(json.utf8))
        XCTAssertEqual(decoded.count, 2)
        XCTAssertEqual(decoded[0].pending.timeoutAction, .stop)
        XCTAssertEqual(decoded[0].pending.ts, 1_700_000_000)
        XCTAssertNil(decoded[1].timeoutSecs)
    }

    func testTimeoutCopyOnlySaysWhatTheHubReported() {
        var a = approval("a", ts: 1000)
        XCTAssertEqual(ApprovalTimeoutText.describe(a), "Waiting for an answer.")
        a.timeoutSecs = 300
        a.timeoutAt = 1300
        a.timeoutAction = .deny
        XCTAssertEqual(ApprovalTimeoutText.describe(a, now: Date(timeIntervalSince1970: 1120)),
                       "If nobody answers within 5m (about 3m left), it’s denied and the thread goes on.")
        a.timeoutAction = .stop
        XCTAssertEqual(ApprovalTimeoutText.describe(a, now: Date(timeIntervalSince1970: 2000)),
                       "The 5m timeout has passed; waiting for the hub. Then it’s denied and the thread is stopped.")
        XCTAssertEqual(ApprovalTimeoutText.duration(45), "45s")
        XCTAssertEqual(ApprovalTimeoutText.duration(5400), "1.5h")
    }

    func testNotificationTextNeverCarriesInputOrReason() {
        XCTAssertEqual(ApprovalNotificationText.title, "Grouper needs your review")
        XCTAssertEqual(ApprovalNotificationText.body(tool: "run_shell", device: "Mac", thread: "Fix tests", chat: "Grouper"),
                       "run_shell on Mac · Fix tests · Grouper")
        XCTAssertEqual(ApprovalNotificationText.identifier(approvalId: "a1"), "grouper-approval-a1")
    }
}
