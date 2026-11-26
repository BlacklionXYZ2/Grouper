import Foundation
import XCTest
@testable import GrouperAPI

final class ThreadPresentationTests: XCTestCase {
    private func event(_ json: String) throws -> Event {
        try JSONDecoder.grouper.decode(Event.self, from: Data(json.utf8))
    }

    private func entry(_ id: String, _ model: String) throws -> ModelEntry {
        try JSONDecoder.grouper.decode(ModelEntry.self, from: Data(#"{"id":"\#(id)","provider_id":"p","model":"\#(model)"}"#.utf8))
    }

    func testFamilyRulesMatchTheWebClient() throws {
        let cases: [(String, ModelFamily)] = [
            ("claude-opus-4", .claude),
            ("anthropic/claude-3.5-sonnet", .claude),
            ("us.anthropic.claude-3-haiku", .claude),
            ("gpt-4o", .gpt),
            ("openai/o3-mini", .gpt),
            ("o1", .gpt),
            ("omni", .unknown),
            ("chatgpt-4o-latest", .gpt),
            ("google/gemma-3-27b-it:free", .gemini),
            ("gemini-2.5-pro", .gemini),
            ("meta-llama/llama-3.1-70b", .llama),
            ("nvidia/llama-3.1-nemotron-70b-instruct", .nemotron),
            ("nemotron-4", .nemotron),
            ("qwen/qwq-32b", .qwen),
            ("qwen3-coder", .qwen),
            ("deepseek-r1", .deepseek),
            ("deepseekr1", .unknown),
            ("mistralai/devstral-small", .mistral),
            ("x-ai/grok-4", .grok),
            ("ernie-4.5", .ernie),
            ("tycho-1", .radium),
            ("fake", .scripted),
            ("claudette", .unknown),
            ("", .unknown),
            ("vendor/", .unknown),
        ]
        for (raw, expected) in cases {
            XCTAssertEqual(ModelFamily.of(model: raw), expected, raw)
        }
        // The provider model string wins over the registry id; the id is the fallback.
        XCTAssertEqual(ModelFamily.of(try entry("fast", "gpt-4o-mini")), .gpt)
        XCTAssertEqual(ModelFamily.of(try entry("claude-local", "my-finetune")), .claude)
        let models = [try entry("opus", "anthropic/claude-opus-4")]
        XCTAssertEqual(ModelFamily.of(registryId: "opus", in: models), .claude)
        XCTAssertEqual(ModelFamily.of(registryId: "gemini-flash", in: models), .gemini)
        XCTAssertEqual(ModelFamily.unknown.glyph, "")
        XCTAssertEqual(ModelFamily.claude.accessibilityLabel, "Claude family")
    }

    func testThreadTranscriptReadsLikeAChat() throws {
        let raws = [
            #"{"seq":1,"conversation_id":"c","thread_id":"t","ts":1,"kind":"thread_started","title":"Task","device_id":"mac","model":"opus","reason":null,"brief":"Do the thing"}"#,
            #"{"seq":2,"conversation_id":"c","thread_id":"t","ts":2,"kind":"thread_activity","event":{"type":"text","text":"Starting"}}"#,
            #"{"seq":3,"conversation_id":"c","thread_id":"t","ts":3,"kind":"thread_activity","event":{"type":"tool_call","call_id":"k1","name":"run_shell","input":{"command":"ls"}}}"#,
            #"{"seq":4,"conversation_id":"c","thread_id":"t","ts":4,"kind":"thread_activity","event":{"type":"tool_result","call_id":"k1","name":"run_shell","ok":true,"output":"a.txt"}}"#,
            #"{"seq":5,"conversation_id":"c","thread_id":"t","ts":5,"kind":"thread_input","from":"user","text":"Use b.txt"}"#,
            #"{"seq":6,"conversation_id":"c","thread_id":"t","ts":6,"kind":"thread_activity","event":{"type":"tool_call","call_id":"k2","name":"ask_orchestrator","input":{"question":"Which folder?"}}}"#,
            #"{"seq":7,"conversation_id":"c","thread_id":"t","ts":7,"kind":"thread_input","from":"orchestrator","text":"The docs folder"}"#,
            #"{"seq":8,"conversation_id":"c","thread_id":"t","ts":8,"kind":"thread_activity","event":{"type":"tool_result","call_id":"k2","name":"ask_orchestrator","ok":true,"output":"The docs folder"}}"#,
            #"{"seq":9,"conversation_id":"c","thread_id":"t","ts":9,"kind":"thread_activity","event":{"type":"text","text":"Done"}}"#,
            #"{"seq":10,"conversation_id":"c","thread_id":"t","ts":10,"kind":"thread_finished","status":"done","summary":"All done"}"#,
        ]
        var folder = TranscriptFolder(conversationId: "c")
        for raw in raws { folder.ingest(try event(raw)) }
        let transcript = ThreadTranscript(activity: folder.activity(threadId: "t"))
        let kinds = transcript.messages.map { message -> String in
            switch message {
            case .inbound(_, let sender, let label, _): return "\(sender == .owner ? "owner" : "orch"):\(label)"
            case .modelLabel: return "who"
            case .text(_, let text): return "text:\(text)"
            case .tool(_, let call, let result): return "tool:\(call.name):\(result?.output ?? "-")"
            case .ask(_, let question): return "ask:\(question)"
            default: return "other"
            }
        }
        XCTAssertEqual(kinds, [
            "orch:Orchestrator · brief",
            "who", "text:Starting", "tool:run_shell:a.txt",
            "owner:You",
            "who", "ask:Which folder?",
            "orch:Orchestrator · answer",
            "who", "text:Done",
        ])
        XCTAssertTrue(transcript.threadSpokeLast)
        XCTAssertEqual(transcript.finishedSummary, "All done")
        XCTAssertEqual(Set(transcript.messages.map(\.id)).count, transcript.messages.count, "ids must be unique for ForEach")
    }

    func testUnmatchedAnswerBecomesItsOwnMessage() throws {
        let raws = [
            #"{"seq":1,"conversation_id":"c","thread_id":"t","ts":1,"kind":"thread_activity","event":{"type":"tool_call","call_id":"k","name":"ask_orchestrator","input":{"other":1}}}"#,
            #"{"seq":2,"conversation_id":"c","thread_id":"t","ts":2,"kind":"thread_activity","event":{"type":"tool_result","call_id":"k","name":"ask_orchestrator","ok":true,"output":"Yes"}}"#,
        ]
        var folder = TranscriptFolder(conversationId: "c")
        for raw in raws { folder.ingest(try event(raw)) }
        let transcript = ThreadTranscript(activity: folder.activity(threadId: "t"))
        XCTAssertEqual(transcript.messages.count, 3)
        guard case .inbound(_, .orchestrator, "Orchestrator · answer", "Yes") = transcript.messages[2] else { return XCTFail("answer missing") }
        XCTAssertFalse(transcript.threadSpokeLast)
    }

    func testElapsedMatchesTheWebFormat() throws {
        XCTAssertEqual(ThreadElapsed.format(42.9), "42s")
        XCTAssertEqual(ThreadElapsed.format(247), "4m 07s")
        XCTAssertEqual(ThreadElapsed.format(7500), "2h 05m")
        XCTAssertEqual(ThreadElapsed.format(273_600), "3d 4h")
        let json = #"{"id":"t","conversation_id":"c","title":"T","device_id":"mac","model":"m","status":"done","reason":null,"summary":null,"created":100,"finished":347}"#
        let done = try JSONDecoder.grouper.decode(ThreadInfo.self, from: Data(json.utf8))
        XCTAssertEqual(done.elapsed().text, "took 4m 07s")
        let runningJSON = json.replacingOccurrences(of: #""done""#, with: #""running""#)
        let running = try JSONDecoder.grouper.decode(ThreadInfo.self, from: Data(runningJSON.utf8))
        let live = running.elapsed(now: Date(timeIntervalSince1970: 160))
        XCTAssertTrue(live.live)
        XCTAssertEqual(live.text, "1m 00s")
    }

    func testLifecycleEventsUpdateThreadRowsAtOnce() throws {
        var threads: [ThreadInfo] = []
        XCTAssertTrue(ThreadInfo.upsert(&threads, with: try event(#"{"seq":1,"conversation_id":"c","thread_id":"t","ts":100,"kind":"thread_started","title":"Task","device_id":"mac","model":"opus","reason":"fast","brief":"do"}"#)))
        XCTAssertEqual(threads.first?.status, "running")
        XCTAssertEqual(threads.first?.created, 100)
        XCTAssertEqual(threads.first?.model, "opus")

        ThreadInfo.upsert(&threads, with: try event(#"{"seq":2,"conversation_id":"c","thread_id":"t","ts":110,"kind":"thread_waiting","device_id":"mac","reason":"offline"}"#))
        XCTAssertEqual(threads.first?.status, "waiting_device")
        ThreadInfo.upsert(&threads, with: try event(#"{"seq":3,"conversation_id":"c","thread_id":"t","ts":120,"kind":"thread_handed_over","device_id":"pc","files":[],"skipped":[],"brief":"go on"}"#))
        XCTAssertEqual(threads.first?.status, "running")
        XCTAssertEqual(threads.first?.deviceId, "pc")
        ThreadInfo.upsert(&threads, with: try event(#"{"seq":4,"conversation_id":"c","thread_id":"t","ts":347,"kind":"thread_finished","status":"done","summary":"All done"}"#))
        XCTAssertEqual(threads.first?.status, "done")
        XCTAssertEqual(threads.first?.summary, "All done")
        XCTAssertEqual(threads.first?.elapsed().text, "took 4m 07s")
        // A late waiting event cannot reopen a finished thread.
        XCTAssertFalse(ThreadInfo.upsert(&threads, with: try event(#"{"seq":5,"conversation_id":"c","thread_id":"t","ts":400,"kind":"thread_waiting","device_id":"mac","reason":"late"}"#)))
        XCTAssertEqual(threads.count, 1)
    }

    func testStaleFetchDoesNotReopenAFinishedThread() throws {
        let done = ThreadInfo(id: "t", conversationId: "c", title: "T", deviceId: "mac", model: "m", status: "done", reason: nil, summary: nil, created: 1, finished: 2)
        let fresh = ThreadInfo(id: "new", conversationId: "c", title: "N", deviceId: "mac", model: "m", status: "running", reason: nil, summary: nil, created: 3, finished: nil)
        let other = ThreadInfo(id: "o", conversationId: "other", title: "O", deviceId: "mac", model: "m", status: "running", reason: nil, summary: nil, created: 1, finished: nil)
        let staleFetch = [ThreadInfo(id: "t", conversationId: "c", title: "T", deviceId: "mac", model: "m", status: "running", reason: nil, summary: nil, created: 1, finished: nil)]
        let merged = ThreadInfo.merge([done, fresh, other], fetched: staleFetch, conversationId: "c")
        XCTAssertEqual(merged.first { $0.id == "t" }?.status, "done")
        XCTAssertNotNil(merged.first { $0.id == "new" }, "a thread the fetch has not seen yet is kept")
        XCTAssertNotNil(merged.first { $0.id == "o" }, "other chats are untouched")
        XCTAssertEqual(merged.count, 3)

        // The chat card keeps a stored thread_finished over a stale running row.
        let raws = [
            #"{"seq":1,"conversation_id":"c","thread_id":"t","ts":1,"kind":"thread_started","title":"T","device_id":"mac","model":"m","reason":null,"brief":"do"}"#,
            #"{"seq":2,"conversation_id":"c","thread_id":"t","ts":2,"kind":"thread_finished","status":"failed","summary":"broke"}"#,
        ]
        let folder = TranscriptFolder(conversationId: "c", events: try raws.map(event))
        guard case .thread(let card) = folder.transcript(threads: staleFetch).first else { return XCTFail("card missing") }
        XCTAssertEqual(card.status, "failed")
    }

    func testProviderListDecodesViaDevice() throws {
        let raw = #"[{"id":"local","kind":"openai","base_url":"http://127.0.0.1:1234/v1","has_key":false,"via_device":"Gaming PC","options":{"via_device":"Gaming PC"}},{"id":"anthropic","kind":"anthropic","base_url":null,"has_key":true}]"#
        let providers = try JSONDecoder.grouper.decode([ProviderInfo].self, from: Data(raw.utf8))
        XCTAssertEqual(providers[0].viaDevice, "Gaming PC")
        XCTAssertNil(providers[1].viaDevice)
        XCTAssertTrue(providers[1].hasKey)
    }
}
