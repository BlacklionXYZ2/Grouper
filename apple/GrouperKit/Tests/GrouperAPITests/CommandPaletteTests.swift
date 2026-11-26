import Foundation
import XCTest
@testable import GrouperAPI

final class CommandPaletteTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        try JSONDecoder.grouper.decode(T.self, from: Data(json.utf8))
    }

    private func catalog() throws -> [PaletteItem] {
        let conversations = try decode([ConversationInfo].self, #"""
        [{"id":"c1","mode":"project","title":"Café plans","created":1,"model":null},
         {"id":"c2","mode":"standard","title":"Quick question","created":2,"model":null},
         {"id":"c3","mode":"project","title":"Plans for the garden","created":3,"model":null}]
        """#)
        let threads = try decode([ThreadInfo].self, #"""
        [{"id":"t1","conversation_id":"c1","title":"Draft the menu","device_id":"mac","model":"claude-opus-4","status":"running","reason":null,"summary":null,"created":1,"finished":null}]
        """#)
        let models = try decode([ModelEntry].self, #"[{"id":"opus","provider_id":"anthropic","model":"claude-opus-4"}]"#)
        let devices = try decode([DeviceInfo].self, #"[{"id":"mac","name":"Studio Mac","platform":"macos","online":true}]"#)
        return CommandPalette.items(conversations: conversations, threads: threads, models: models, devices: devices,
                                    actions: [.newChat, .newProjectChat, .newStandardChat], pages: [.devices, .settings])
    }

    func testNormalizationIgnoresCaseAccentsAndPunctuation() {
        XCTAssertEqual(CommandPalette.normalize("  Café — PLANS!! "), "cafe plans")
        XCTAssertEqual(CommandPalette.tokens("...!"), [])
    }

    func testRowsCarryTheirDisambiguators() throws {
        let items = try catalog()
        let thread = try XCTUnwrap(items.first { $0.id == "thread:t1" })
        XCTAssertEqual(thread.subtitle, "In Café plans · Studio Mac · claude-opus-4 · Running")
        XCTAssertEqual(thread.target, .thread(id: "t1", conversationId: "c1"))
        XCTAssertEqual(items.first { $0.id == "chat:c2" }?.subtitle, "Standard chat")
        XCTAssertEqual(items.first { $0.id == "device:mac" }?.subtitle, "Online · macos")
        XCTAssertEqual(items.first { $0.id == "model:opus" }?.subtitle, "claude-opus-4 · anthropic")
    }

    func testGroupsComeInSpecOrderAndTitlePrefixBeatsRecency() throws {
        let items = try catalog()
        let sections = CommandPalette.sections(items, query: "plans", recents: ["chat:c3"])
        XCTAssertEqual(sections.map(\.title), ["Chats", "Threads"], "threads also match their chat's name")
        // "Café plans" only matches by a later word; "Plans for the garden" starts with it.
        XCTAssertEqual(sections[0].items.map(\.id), ["chat:c3", "chat:c1"])

        let cafe = CommandPalette.sections(items, query: "cafe", recents: [])
        XCTAssertEqual(cafe.map(\.title), ["Chats", "Threads"], "the thread matches through its chat's name")
        XCTAssertEqual(cafe[1].items.map(\.id), ["thread:t1"])

        let new = CommandPalette.sections(items, query: "new", recents: ["action:newStandardChat"])
        XCTAssertEqual(new.first?.title, "Actions")
        XCTAssertEqual(new.first?.items.first?.id, "action:newStandardChat", "equal matches: recent first")
        XCTAssertEqual(new.first?.items.dropFirst().map(\.id), ["action:newChat", "action:newProjectChat"], "then by title")
    }

    func testTokensMatchInAnyOrder() throws {
        let items = try catalog()
        let sections = CommandPalette.sections(items, query: "garden plans", recents: [])
        XCTAssertEqual(CommandPalette.flatten(sections).map(\.id), ["chat:c3"])
        XCTAssertTrue(CommandPalette.sections(items, query: "zebra", recents: []).isEmpty)
    }

    func testEmptyQueryShowsRecentsThenActionsAndPages() throws {
        let items = try catalog()
        let sections = CommandPalette.sections(items, query: "  ", recents: ["chat:c2", "page:settings", "chat:deleted"])
        XCTAssertEqual(sections.map(\.title), ["Recent", "Actions", "Pages"])
        XCTAssertEqual(sections[0].items.map(\.id), ["chat:c2", "page:settings"], "unknown ids are skipped")
        XCTAssertEqual(sections[2].items.map(\.id), ["page:devices"], "a recent row isn't listed twice")
    }

    func testGroupCapAndKeyboardMovement() throws {
        let many = (0..<12).map { PaletteItem(target: .chat(id: "c\($0)"), title: "Chat \($0)", subtitle: "Project chat") }
        let sections = CommandPalette.sections(many, query: "chat", recents: [])
        XCTAssertEqual(sections[0].items.count, 8)
        XCTAssertEqual(sections[0].total, 12)

        let rows = CommandPalette.flatten(sections)
        XCTAssertEqual(CommandPalette.resolveSelection(nil, in: rows), rows.first?.id)
        XCTAssertEqual(CommandPalette.resolveSelection("missing", in: rows), rows.first?.id)
        XCTAssertEqual(CommandPalette.move(rows[0].id, by: -1, in: rows), rows[0].id, "Up stops at the top")
        XCTAssertEqual(CommandPalette.move(rows[0].id, by: 1, in: rows), rows[1].id)
        XCTAssertEqual(CommandPalette.move(rows[0].id, by: Int.max, in: rows), rows.last?.id)
        XCTAssertEqual(CommandPalette.move(rows.last?.id, by: 1, in: rows), rows.last?.id, "Down stops at the bottom")
        XCTAssertNil(CommandPalette.move(nil, by: 1, in: []))
    }

    func testRecentsAreBoundedAndDeduplicated() {
        var recents: [String] = []
        for id in ["a", "b", "c", "a", "d", "e", "f"] { recents = CommandPalette.remember(id, in: recents) }
        XCTAssertEqual(recents, ["f", "e", "d", "a", "c"])
    }
}
