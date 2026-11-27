import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
@testable import GrouperAPI

final class HubMoveTests: XCTestCase {
    // MARK: - Decoding

    func testDecodesHubIdentityIncludingLegacyShape() throws {
        let full = #"{"hub_id":"h_abc","hub_version":"0.2.0","protocol_version":6,"account":"oscar@example.com","hub_device_id":"d_win1","epoch":3,"key_sha256":"deadbeef"}"#
        let identity = try JSONDecoder.grouper.decode(HubIdentity.self, from: Data(full.utf8))
        XCTAssertEqual(identity.hubId, "h_abc")
        XCTAssertEqual(identity.hubVersion, "0.2.0")
        XCTAssertEqual(identity.protocolVersion, 6)
        XCTAssertEqual(identity.account, "oscar@example.com")
        XCTAssertEqual(identity.hubDeviceId, "d_win1")
        XCTAssertEqual(identity.epoch, 3)
        XCTAssertEqual(identity.keySha256, "deadbeef")

        // Pre-local-first hubs omit account, hub_device_id, epoch, and key_sha256.
        let legacy = #"{"hub_id":"h_old","hub_version":"0.1.0","protocol_version":6}"#
        let oldIdentity = try JSONDecoder.grouper.decode(HubIdentity.self, from: Data(legacy.utf8))
        XCTAssertEqual(oldIdentity.hubId, "h_old")
        XCTAssertNil(oldIdentity.account)
        XCTAssertEqual(oldIdentity.hubDeviceId, "")
        XCTAssertEqual(oldIdentity.epoch, 0)
        XCTAssertNil(oldIdentity.keySha256)
    }

    func testDecodesInstallDevicesAndSeparatesEligibleFromPhonesAndBrowsers() throws {
        let raw = #"""
        {
          "hub_device_id": "d_win",
          "epoch": 2,
          "hub_version": "0.2.0",
          "hub_since": 1795600000,
          "estimated_snapshot_bytes": 15728640,
          "installs": [
            {
              "device_id": "d_win",
              "device_ids": ["d_win"],
              "node_device_id": "d_win",
              "install_id": "win_install_0001",
              "name": "Oscar's PC",
              "platform": "windows",
              "is_hub": true,
              "online": true,
              "last_seen": 1795650000,
              "host": {"can_host": true, "app_version": "0.2.0", "cloudflared": true},
              "eligible": true,
              "selectable": false,
              "reason": "current_hub"
            },
            {
              "device_id": "d_mac_node",
              "device_ids": ["d_mac_admin", "d_mac_node"],
              "node_device_id": "d_mac_node",
              "install_id": "mac_install_0001",
              "name": "Oscar's Mac",
              "platform": "macos",
              "is_hub": false,
              "online": true,
              "last_seen": 1795650000,
              "host": {"can_host": true, "app_version": "0.2.0", "free_bytes": 50000000000, "cloudflared": false},
              "eligible": true,
              "selectable": true,
              "warnings": ["Remote access will stop until cloudflared is installed there"]
            },
            {
              "device_id": "d_linux",
              "device_ids": ["d_linux"],
              "node_device_id": "d_linux",
              "name": "Home Server",
              "platform": "linux",
              "is_hub": false,
              "online": false,
              "last_seen": 1795640000,
              "host": {"can_host": true, "app_version": "0.2.0", "cloudflared": true},
              "eligible": true,
              "selectable": false,
              "reason": "offline"
            },
            {
              "device_id": "d_old_mac",
              "device_ids": ["d_old_mac"],
              "node_device_id": "d_old_mac",
              "name": "Old Mac",
              "platform": "macos",
              "is_hub": false,
              "online": true,
              "host": {"can_host": true, "app_version": "0.1.0", "cloudflared": true},
              "eligible": true,
              "selectable": false,
              "reason": "older_version"
            },
            {
              "device_id": "d_iphone",
              "device_ids": ["d_iphone"],
              "install_id": "ios_install_0001",
              "name": "Oscar's iPhone",
              "platform": "ios",
              "is_hub": false,
              "online": false,
              "eligible": false,
              "selectable": false,
              "reason": "cannot_host"
            },
            {
              "device_id": "d_web",
              "device_ids": ["d_web"],
              "name": "Safari",
              "platform": "web",
              "is_hub": false,
              "online": false,
              "eligible": false,
              "selectable": false,
              "reason": "cannot_host"
            }
          ]
        }
        """#

        let decoded = try JSONDecoder.grouper.decode(InstallDevicesResponse.self, from: Data(raw.utf8))
        XCTAssertEqual(decoded.hubDeviceId, "d_win")
        XCTAssertEqual(decoded.epoch, 2)
        XCTAssertEqual(decoded.hubSince, 1_795_600_000)
        XCTAssertEqual(decoded.estimatedSnapshotBytes, 15_728_640)
        XCTAssertEqual(decoded.currentHub?.name, "Oscar's PC")
        XCTAssertEqual(decoded.eligibleInstalls.map(\.name), ["Oscar's PC", "Oscar's Mac", "Home Server", "Old Mac"])
        XCTAssertEqual(decoded.ineligibleInstalls.map(\.name), ["Oscar's iPhone", "Safari"])

        let mac = try XCTUnwrap(decoded.installs.first { $0.deviceId == "d_mac_node" })
        XCTAssertTrue(mac.canBeHubTarget)
        XCTAssertEqual(mac.warnings, ["Remote access will stop until cloudflared is installed there"])
        XCTAssertEqual(mac.host?.freeBytes, 50_000_000_000)
        XCTAssertEqual(mac.host?.cloudflared, false)

        let iphone = try XCTUnwrap(decoded.installs.first { $0.deviceId == "d_iphone" })
        XCTAssertFalse(iphone.canBeHubTarget)
        XCTAssertEqual(iphone.parsedReason, .cannotHost)
        XCTAssertEqual(iphone.warnings, [])

        // Even if a buggy server marked an iPhone selectable, client-side guard refuses it as a target.
        let badPhone = InstallDevice(
            deviceId: "d_bad_ios", name: "Phone", platform: "ios",
            isHub: false, online: true, eligible: true, selectable: true
        )
        XCTAssertFalse(badPhone.canBeHubTarget)
    }

    func testDecodesMoveStatusAcrossPhases() throws {
        let idleRaw = #"{"state":"idle"}"#
        let idle = try JSONDecoder.grouper.decode(MoveStatus.self, from: Data(idleRaw.utf8))
        XCTAssertEqual(idle.state, .idle)
        XCTAssertFalse(idle.state.isActive)
        XCTAssertFalse(idle.state.canCancel)

        let copyingRaw = #"""
        {
          "move_id": "m_1234",
          "state": "copying",
          "target_device_id": "d_mac",
          "target_name": "Oscar's Mac",
          "threads_mode": "wait",
          "progress_pct": 62,
          "snapshot_bytes": 2097152,
          "message": "Copying snapshot…",
          "started_at": 1795650000,
          "updated_at": 1795650012
        }
        """#
        let copying = try JSONDecoder.grouper.decode(MoveStatus.self, from: Data(copyingRaw.utf8))
        XCTAssertEqual(copying.moveId, "m_1234")
        XCTAssertEqual(copying.state, .copying)
        XCTAssertTrue(copying.state.isActive)
        XCTAssertTrue(copying.state.canCancel)
        XCTAssertEqual(copying.threadsMode, .wait)
        XCTAssertEqual(copying.progressPct, 62)
        XCTAssertEqual(copying.summaryLine, "Moving your hub to Oscar's Mac… copying (62%)")

        let switchingRaw = #"{"move_id":"m_1234","state":"switching","target_name":"Oscar's Mac","progress_pct":90}"#
        let switching = try JSONDecoder.grouper.decode(MoveStatus.self, from: Data(switchingRaw.utf8))
        XCTAssertTrue(switching.state.isActive)
        XCTAssertFalse(switching.state.canCancel)

        let rolledBackRaw = #"{"move_id":"m_1234","state":"rolled_back","target_name":"Oscar's Mac","error":"target disconnected","new_epoch":4}"#
        let rolledBack = try JSONDecoder.grouper.decode(MoveStatus.self, from: Data(rolledBackRaw.utf8))
        XCTAssertEqual(rolledBack.state, .rolledBack)
        XCTAssertFalse(rolledBack.state.isActive)
        XCTAssertEqual(rolledBack.error, "target disconnected")
        XCTAssertEqual(rolledBack.newEpoch, 4)
        XCTAssertEqual(rolledBack.summaryLine, "target disconnected")
    }

    func testParsesHubMoveProgressLiveEvent() throws {
        let raw = #"""
        {
          "seq": 0,
          "conversation_id": "",
          "thread_id": null,
          "ts": 1795650015,
          "kind": "hub_move_progress",
          "move_id": "m_99",
          "state": "copying",
          "target_device_id": "d_mac",
          "target_name": "Oscar's Mac",
          "progress_pct": 75,
          "message": "Copying snapshot…"
        }
        """#
        let event = try JSONDecoder.grouper.decode(Event.self, from: Data(raw.utf8))
        let update = try XCTUnwrap(HubMoveProgressUpdate(event: event))
        XCTAssertEqual(update.moveId, "m_99")
        XCTAssertEqual(update.state, .copying)
        XCTAssertEqual(update.progressPct, 75)
        let merged = update.applying(to: MoveStatus(moveId: "m_99", state: .preparing, snapshotBytes: 1024))
        XCTAssertEqual(merged.state, .copying)
        XCTAssertEqual(merged.progressPct, 75)
        XCTAssertEqual(merged.snapshotBytes, 1024)
        XCTAssertEqual(merged.targetName, "Oscar's Mac")
    }

    // MARK: - Identity comparison (moved, stale epoch, replaced)

    func testHubIdentityComparison() {
        let initial = HubIdentity(hubId: "h1", hubVersion: "0.2.0", protocolVersion: 6, account: "a@b.c", hubDeviceId: "d1", epoch: 1)
        XCTAssertEqual(initial.compare(against: nil, now: 100), .initial)

        let cached = CachedHubIdentity(identity: initial, recordedAt: 100)
        XCTAssertEqual(initial.compare(against: cached, now: 200), .unchanged)

        let moved = HubIdentity(hubId: "h1", hubVersion: "0.2.0", protocolVersion: 6, account: "a@b.c", hubDeviceId: "d2", epoch: 2)
        XCTAssertEqual(moved.compare(against: cached, now: 200), .moved(previousDeviceId: "d1", newDeviceId: "d2", newEpoch: 2))

        let cachedEpoch3 = CachedHubIdentity(hubId: "h1", account: "a@b.c", hubDeviceId: "d2", epoch: 3, recordedAt: 200)
        let stale = HubIdentity(hubId: "h1", hubVersion: "0.2.0", protocolVersion: 6, account: "a@b.c", hubDeviceId: "d1", epoch: 2)
        XCTAssertEqual(stale.compare(against: cachedEpoch3, now: 250), .staleEpoch(cachedEpoch: 3, offeredEpoch: 2))

        let replaced = HubIdentity(hubId: "h_new", hubVersion: "0.2.0", protocolVersion: 6, account: "a@b.c", hubDeviceId: "d9", epoch: 1)
        XCTAssertEqual(replaced.compare(against: cachedEpoch3, now: 300), .replaced(previousHubId: "h1", newHubId: "h_new", detectedAt: 300))
    }

    // MARK: - Formatting helpers

    func testFormattingHelpers() {
        XCTAssertEqual(HubMoveFormatting.formattedBytes(512), "512 B")
        XCTAssertEqual(HubMoveFormatting.formattedBytes(2048), "2.0 KB")
        XCTAssertEqual(HubMoveFormatting.formattedBytes(15 * 1024 * 1024), "15.0 MB")
        XCTAssertEqual(HubMoveFormatting.estimatedDuration(snapshotBytes: 1_000_000), "Less than a minute")
        XCTAssertEqual(HubMoveFormatting.estimatedDuration(snapshotBytes: 500 * 1024 * 1024), "About 2 minutes")

        let utc = TimeZone(secondsFromGMT: 0)!
        // 2026-11-26 21:40:00 UTC
        let ts: UInt64 = 1_795_729_200
        XCTAssertEqual(HubMoveFormatting.hubSinceLabel(timestamp: ts, timeZone: utc), "Hub since 26 Nov")
        XCTAssertEqual(HubMoveFormatting.shortTime(timestamp: ts, timeZone: utc), "21:40")

        let offline = InstallDevice(
            deviceId: "d_lin", name: "Linux", platform: "linux",
            isHub: false, online: false, lastSeen: ts, eligible: true, selectable: false, reason: "offline"
        )
        XCTAssertEqual(
            HubMoveFormatting.statusSubtitle(for: offline, timeZone: utc),
            "Offline · last seen 21:40. Turn it on to move the hub there."
        )

        let older = InstallDevice(
            deviceId: "d_old", name: "Old", platform: "macos",
            isHub: false, online: true, eligible: true, selectable: false, reason: "older_version"
        )
        XCTAssertEqual(
            HubMoveFormatting.statusSubtitle(for: older, timeZone: utc),
            "Update Grouper on this device first."
        )
    }

    // MARK: - URLProtocol HTTP client tests

    func testHubMoveClientEndpointsAndPayloads() async throws {
        let session = HubMoveStub.session { request in
            switch (request.httpMethod, request.url?.path) {
            case ("GET"?, "/v1/hub"?):
                return (200, #"{"hub_id":"h_1","hub_version":"0.2.0","protocol_version":6,"account":"oscar@example.com","hub_device_id":"d_win","epoch":1}"#)
            case ("GET"?, "/v1/devices/installs"?):
                return (200, #"{"hub_device_id":"d_win","epoch":1,"hub_version":"0.2.0","hub_since":1795600000,"estimated_snapshot_bytes":4096,"installs":[{"device_id":"d_win","device_ids":["d_win"],"name":"Oscar's PC","platform":"windows","is_hub":true,"online":true,"eligible":true,"selectable":false,"reason":"current_hub"},{"device_id":"d_mac","device_ids":["d_mac"],"node_device_id":"d_mac","name":"Oscar's Mac","platform":"macos","is_hub":false,"online":true,"eligible":true,"selectable":true}]}"#)
            case ("GET"?, "/v1/hub/move"?):
                return (200, #"{"state":"idle"}"#)
            case ("POST"?, "/v1/hub/move"?):
                return (202, #"{"move_id":"m_1","state":"preparing","target_device_id":"d_mac","target_name":"Oscar's Mac","threads_mode":"stop","progress_pct":0}"#)
            case ("DELETE"?, "/v1/hub/move"?):
                return (200, #"{"move_id":"m_1","state":"rolled_back","target_device_id":"d_mac","target_name":"Oscar's Mac","error":"cancelled"}"#)
            default:
                return (404, #"{"error":"not found"}"#)
            }
        }

        let client = try HubMoveClient(baseURL: "https://hub.test", token: "admin.secret", session: session)

        let info = try await client.hubIdentity()
        XCTAssertEqual(info.hubDeviceId, "d_win")
        XCTAssertEqual(info.account, "oscar@example.com")
        XCTAssertEqual(info.epoch, 1)

        let installs = try await client.installDevices()
        XCTAssertEqual(installs.installs.count, 2)
        XCTAssertEqual(installs.currentHub?.name, "Oscar's PC")

        let initialMove = try await client.moveStatus()
        XCTAssertEqual(initialMove.state, .idle)

        let started = try await client.startMove(targetDevice: "d_mac", idToken: "eyJ.fresh.token", threads: .stop)
        XCTAssertEqual(started.moveId, "m_1")
        XCTAssertEqual(started.state, .preparing)
        XCTAssertEqual(started.threadsMode, .stop)

        let cancelled = try await client.cancelMove()
        XCTAssertEqual(cancelled.state, .rolledBack)

        XCTAssertEqual(HubMoveStub.requests.map(\.path), [
            "/v1/hub",
            "/v1/devices/installs",
            "/v1/hub/move",
            "/v1/hub/move",
            "/v1/hub/move",
        ])
        XCTAssertNil(HubMoveStub.requests[0].authorization, "GET /v1/hub is public and sends no token")
        XCTAssertTrue(HubMoveStub.requests.dropFirst().allSatisfy { $0.authorization == "Bearer admin.secret" })

        let post = try XCTUnwrap(HubMoveStub.requests.first { $0.method == "POST" })
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: post.body) as? [String: String])
        XCTAssertEqual(body, [
            "target_device": "d_mac",
            "id_token": "eyJ.fresh.token",
            "threads": "stop",
        ])
    }

    func testHubMoveClientMapsConflictAndMoving503Errors() async throws {
        let session = HubMoveStub.session { request in
            switch (request.httpMethod, request.url?.path) {
            case ("POST"?, "/v1/hub/move"?):
                return (409, #"{"error":"Oscar's Mac is offline; turn it on to move the hub there"}"#)
            case ("DELETE"?, "/v1/hub/move"?):
                return (503, #"{"error":"Your hub is moving"}"#)
            default:
                return (404, #"{"error":"not found"}"#)
            }
        }
        let client = try HubMoveClient(baseURL: "https://hub.test", token: "admin.secret", session: session)

        do {
            _ = try await client.startMove(targetDevice: "d_mac", idToken: "tok")
            XCTFail("expected 409")
        } catch {
            XCTAssertEqual(error as? HubClientError, .conflict("Oscar's Mac is offline; turn it on to move the hub there"))
            XCTAssertFalse(HubMoveClient.hubLacksMoveAPI(error))
        }

        do {
            _ = try await client.cancelMove()
            XCTFail("expected 503")
        } catch {
            XCTAssertTrue(HubMoveClient.isHubMovingError(error))
        }

        XCTAssertTrue(HubMoveClient.hubLacksMoveAPI(HubClientError.notFound("not found")))
        XCTAssertTrue(HubMoveClient.hubLacksMoveAPI(HubClientError.server(status: 405, message: "")))
    }

    func testStartMoveRefusesABlankGoogleIDTokenWithoutPosting() async throws {
        let session = HubMoveStub.session { _ in
            (500, #"{"error":"should not be called"}"#)
        }
        let client = try HubMoveClient(baseURL: "https://hub.test", token: "admin.secret", session: session)
        do {
            _ = try await client.startMove(targetDevice: "d_mac", idToken: "  ")
            XCTFail("expected a blank id_token to be refused")
        } catch {
            XCTAssertEqual(
                error as? HubClientError,
                .badRequest("A fresh Google sign-in is required to move the hub.")
            )
        }
        XCTAssertTrue(HubMoveStub.requests.isEmpty)
    }
}

/// Answers requests from a closure and records them, matching the existing URLProtocol stubs.
private final class HubMoveStub: URLProtocol {
    struct Seen {
        let method: String
        let path: String
        let authorization: String?
        let body: Data
    }

    nonisolated(unsafe) static var handler: ((URLRequest) -> (Int, String))?
    nonisolated(unsafe) static var requests: [Seen] = []

    static func session(_ handler: @escaping (URLRequest) -> (Int, String)) -> URLSession {
        self.handler = handler
        requests = []
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [HubMoveStub.self]
        return URLSession(configuration: configuration)
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let body = request.httpBody ?? request.httpBodyStream.map(Self.read) ?? Data()
        Self.requests.append(Seen(
            method: request.httpMethod ?? "GET",
            path: request.url?.path ?? "",
            authorization: request.value(forHTTPHeaderField: "Authorization"),
            body: body
        ))
        let (status, text) = Self.handler?(request) ?? (500, "")
        let response = HTTPURLResponse(
            url: request.url!,
            statusCode: status,
            httpVersion: "HTTP/1.1",
            headerFields: ["Content-Type": "application/json"]
        )!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(text.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}

    private static func read(_ stream: InputStream) -> Data {
        stream.open()
        defer { stream.close() }
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            if count <= 0 { break }
            data.append(buffer, count: count)
        }
        return data
    }
}
