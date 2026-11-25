import Foundation
import XCTest
@testable import GrouperAPI

final class FoldingTests: XCTestCase {
    private func event(_ json: String) throws -> Event {
        try JSONDecoder.grouper.decode(Event.self, from: Data(json.utf8))
    }

    func testTranscriptFoldsThreadInPlaceAndScopesConversation() throws {
        let events = try [
            #"{"seq":1,"conversation_id":"c1","thread_id":null,"ts":101,"kind":"user_message","text":"two haiku"}"#,
            #"{"seq":2,"conversation_id":"c1","thread_id":"t1","ts":102,"kind":"thread_started","title":"Haiku Mac","device_id":"mac","model":"fake","reason":null,"brief":"write it"}"#,
            #"{"seq":3,"conversation_id":"c1","thread_id":"t1","ts":103,"kind":"thread_activity","event":{"type":"text","text":"drafting"}}"#,
            #"{"seq":4,"conversation_id":"c1","thread_id":"t1","ts":104,"kind":"approval_requested","approval_id":"a1","device_id":"mac","tool":"run_shell","input":{"command":"ls"},"reason":"shell"}"#,
            #"{"seq":5,"conversation_id":"c2","thread_id":"other","ts":105,"kind":"thread_started","title":"Wrong chat","device_id":"hub","model":"fake","reason":null,"brief":"wrong"}"#
        ].map(event)
        var folder = TranscriptFolder(conversationId: "c1")
        for item in events { folder.ingest(item) }

        let rows = folder.transcript(threads: [])
        XCTAssertEqual(rows.count, 2)
        guard case .thread(let card) = rows[1] else { return XCTFail("thread card missing") }
        XCTAssertEqual(card.title, "Haiku Mac")
        XCTAssertEqual(card.latestText, "drafting")
        XCTAssertEqual(card.pendingApprovals, 1)
        XCTAssertFalse(rows.contains { if case .thread(let c) = $0 { return c.threadId == "other" }; return false })
    }

    func testLiveDeltasStayOutOfDurableEventsAndStoredTextClearsDraft() throws {
        var folder = TranscriptFolder(conversationId: "c")
        XCTAssertFalse(folder.ingest(try event(#"{"seq":0,"conversation_id":"c","thread_id":null,"ts":1,"kind":"assistant_delta","text":"Hel"}"#)))
        XCTAssertEqual(folder.assistantDraft, "Hel")
        XCTAssertTrue(folder.storedEvents.isEmpty)

        XCTAssertTrue(folder.ingest(try event(#"{"seq":1,"conversation_id":"c","thread_id":null,"ts":2,"kind":"assistant_message","text":"Hello"}"#)))
        XCTAssertEqual(folder.assistantDraft, "")
        XCTAssertEqual(folder.storedEvents.count, 1)
    }

    func testActivityPairsToolResultAndApprovalResolution() throws {
        let raws = [
            #"{"seq":1,"conversation_id":"c","thread_id":"t","ts":1,"kind":"thread_activity","event":{"type":"tool_call","call_id":"k1","name":"run_shell","input":{"command":"ls"}}}"#,
            #"{"seq":2,"conversation_id":"c","thread_id":"t","ts":2,"kind":"approval_requested","approval_id":"a1","device_id":"mac","tool":"run_shell","input":{"command":"ls"},"reason":"manual"}"#,
            #"{"seq":3,"conversation_id":"c","thread_id":"t","ts":3,"kind":"thread_activity","event":{"type":"tool_result","call_id":"k1","name":"run_shell","ok":false,"output":"denied"}}"#,
            #"{"seq":4,"conversation_id":"c","thread_id":"t","ts":4,"kind":"approval_resolved","approval_id":"a1","approved":false}"#
        ]
        var folder = TranscriptFolder(conversationId: "c")
        for raw in raws { folder.ingest(try event(raw)) }
        let activity = folder.activity(threadId: "t")
        XCTAssertEqual(activity.count, 2)
        guard case .tool(_, _, _, let result) = activity[0] else { return XCTFail("tool missing") }
        XCTAssertEqual(result?.output, "denied")
        guard case .approval(_, _, _, let approved) = activity[1] else { return XCTFail("approval missing") }
        XCTAssertEqual(approved, false)
    }

    func testThreadRowWinsOverStaleEventStatus() throws {
        let started = try event(#"{"seq":1,"conversation_id":"c","thread_id":"t","ts":1,"kind":"thread_started","title":"Task","device_id":"mac","model":"fake","reason":null,"brief":"do"}"#)
        let threadJSON = #"{"id":"t","conversation_id":"c","title":"Task","device_id":"mac","model":"fake","status":"stopped","reason":null,"summary":"stopped by you","created":1,"finished":2}"#
        let thread = try JSONDecoder.grouper.decode(ThreadInfo.self, from: Data(threadJSON.utf8))
        let folder = TranscriptFolder(conversationId: "c", events: [started])
        guard case .thread(let card) = folder.transcript(threads: [thread]).first else { return XCTFail("thread missing") }
        XCTAssertEqual(card.status, "stopped")
        XCTAssertEqual(card.summary, "stopped by you")
    }
}
