import Foundation
import XCTest
@testable import GrouperAPI

final class DecodingTests: XCTestCase {
    func testDecodesRealHubEventShapes() throws {
        let samples = [
            #"{"seq":1,"conversation_id":"c","thread_id":null,"ts":101,"kind":"assistant_message","text":"hi"}"#,
            #"{"seq":0,"conversation_id":"c","thread_id":"t","ts":102,"kind":"thread_activity","event":{"type":"text_delta","text":"par"}}"#,
            #"{"seq":8,"conversation_id":"c","thread_id":"t","ts":108,"kind":"approval_requested","approval_id":"a1","device_id":"laptop","tool":"run_shell","input":{"command":"ls"},"reason":"shell is set to Manual"}"#
        ]
        let events = try samples.map { try JSONDecoder.grouper.decode(Event.self, from: Data($0.utf8)) }

        XCTAssertEqual(events[0].body, .assistantMessage("hi"))
        XCTAssertEqual(events[1].body, .threadActivity(.textDelta("par")))
        guard case .approvalRequested(let approval) = events[2].body else { return XCTFail("approval not decoded") }
        XCTAssertEqual(approval.approvalId, "a1")
        XCTAssertEqual(approval.input, .object(["command": .string("ls")]))
    }

    func testUnknownEventKindIsForwardCompatible() throws {
        let raw = #"{"seq":21,"conversation_id":"c","thread_id":null,"ts":200,"kind":"future_kind","answer":42}"#
        let event = try JSONDecoder.grouper.decode(Event.self, from: Data(raw.utf8))
        guard case .unknown(let kind, let payload) = event.body else { return XCTFail("unknown kind was rejected") }
        XCTAssertEqual(kind, "future_kind")
        XCTAssertEqual(payload["answer"], .number(42))
    }

    func testDecodesRegistryAndDeviceDefaults() throws {
        let deviceRaw = #"{"id":"mac","name":"MacBook","platform":"macos","online":true,"capabilities":[{"name":"mac_info","description":"Mac info"}],"last_seen":123}"#
        let device = try JSONDecoder.grouper.decode(DeviceInfo.self, from: Data(deviceRaw.utf8))
        XCTAssertEqual(device.folders, [])
        XCTAssertEqual(device.capabilities.first?.name, "mac_info")
        // An older hub says nothing about machines: one credential, not known to run threads.
        XCTAssertEqual(device.aliases, [])
        XCTAssertFalse(device.admin)
        XCTAssertNil(device.runsThreads)

        // The hub lists a machine once: the node's id leads, the app's sign-in is an alias.
        let machineRaw = #"{"id":"node1","name":"MacBook","platform":"macos","online":true,"capabilities":[],"last_seen":null,"aliases":["app1"],"admin":true,"runs_threads":true}"#
        let machine = try JSONDecoder.grouper.decode(DeviceInfo.self, from: Data(machineRaw.utf8))
        XCTAssertEqual(machine.aliases, ["app1"])
        XCTAssertTrue(machine.admin)
        XCTAssertEqual(machine.runsThreads, true)

        let modelRaw = #"{"id":"opus","provider_id":"anthropic","model":"claude-opus","roles":["orchestrator"],"free":false}"#
        let model = try JSONDecoder.grouper.decode(ModelEntry.self, from: Data(modelRaw.utf8))
        XCTAssertEqual(model.inputPrice, 0)
        XCTAssertEqual(model.modalities, [])
        XCTAssertEqual(model.kind, "api")
        XCTAssertNil(model.deviceId)
        XCTAssertFalse(model.canOrchestrate)
        XCTAssertFalse(model.isHarness)

        let harnessRaw = #"{"id":"claude-code-pc-opus","provider_id":"claude-code@pc","model":"opus","kind":"harness","device_id":"pc","can_orchestrate":true,"display_name":"Claude Opus"}"#
        let harness = try JSONDecoder.grouper.decode(ModelEntry.self, from: Data(harnessRaw.utf8))
        XCTAssertEqual(harness.kind, "harness")
        XCTAssertEqual(harness.deviceId, "pc")
        XCTAssertTrue(harness.canOrchestrate)
        XCTAssertTrue(harness.isHarness)
        XCTAssertEqual(harness.displayName, "Claude Opus")
    }

    func testEventRoundTripKeepsFlattenedShape() throws {
        let raw = #"{"seq":11,"conversation_id":"c","thread_id":"t","ts":110,"kind":"thread_waiting","device_id":"mac","reason":"offline"}"#
        let event = try JSONDecoder.grouper.decode(Event.self, from: Data(raw.utf8))
        let encoded = try JSONEncoder.grouper.encode(event)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
        XCTAssertEqual(object["kind"] as? String, "thread_waiting")
        XCTAssertEqual(object["device_id"] as? String, "mac")
        XCTAssertEqual(object["conversation_id"] as? String, "c")
    }
}
