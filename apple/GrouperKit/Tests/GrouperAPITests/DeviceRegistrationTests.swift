import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
@testable import GrouperAPI

final class DeviceRegistrationTests: XCTestCase {
    func testDecodesRegisteredDevice() throws {
        // As `POST /v1/devices` answers (empty folders are left out, like GET /v1/devices).
        let raw = #"{"device":{"id":"a1b2c3d4e5f6","name":"Oscar's MacBook Pro","platform":"macos","online":false,"capabilities":[],"last_seen":null},"token":"a1b2c3d4e5f6.00ff"}"#
        let registered = try JSONDecoder.grouper.decode(RegisteredDevice.self, from: Data(raw.utf8))
        XCTAssertEqual(registered.device.id, "a1b2c3d4e5f6")
        XCTAssertEqual(registered.device.name, "Oscar's MacBook Pro")
        XCTAssertEqual(registered.device.platform, "macos")
        XCTAssertFalse(registered.device.online)
        XCTAssertNil(registered.device.lastSeen)
        XCTAssertEqual(registered.device.folders, [])
        XCTAssertEqual(registered.token, "a1b2c3d4e5f6.00ff")
        XCTAssertEqual(DeviceRegistration.deviceId(ofToken: registered.token), registered.device.id)
    }

    func testDecodesRevocation() throws {
        let raw = #"{"revoked":"a1b2c3d4e5f6"}"#
        let revoked = try JSONDecoder.grouper.decode(DeviceRevocation.self, from: Data(raw.utf8))
        XCTAssertEqual(revoked.revoked, "a1b2c3d4e5f6")
    }

    func testDeviceIdOfToken() {
        XCTAssertEqual(DeviceRegistration.deviceId(ofToken: " abc.def \n"), "abc")
        XCTAssertNil(DeviceRegistration.deviceId(ofToken: "abcdef"))
        XCTAssertNil(DeviceRegistration.deviceId(ofToken: ".def"))
        XCTAssertNil(DeviceRegistration.deviceId(ofToken: "abc."))
        XCTAssertNil(DeviceRegistration.deviceId(ofToken: ""))
    }

    func testNamesTheHubAccepts() {
        XCTAssertEqual(DeviceRegistration.name("  Oscar’s MacBook Air  "), "Oscar’s MacBook Air")
        XCTAssertEqual(DeviceRegistration.name(nil), "Mac")
        XCTAssertEqual(DeviceRegistration.name("   "), "Mac")
        XCTAssertEqual(DeviceRegistration.name("Hub"), "Mac")
        XCTAssertEqual(DeviceRegistration.name("Desk\tMac\n"), "DeskMac")
        let long = String(repeating: "é", count: 80)
        XCTAssertEqual(DeviceRegistration.name(long).count, 64)
        XCTAssertEqual(DeviceRegistration.name("", fallback: "Box"), "Box")
    }

    func testSameHub() {
        XCTAssertTrue(DeviceRegistration.sameHub("https://Hub.example/", " https://hub.example"))
        XCTAssertFalse(DeviceRegistration.sameHub("https://hub.example", "https://other.example"))
        XCTAssertFalse(DeviceRegistration.sameHub("", ""))
    }

    func testOldHubIsRecognised() {
        XCTAssertTrue(DeviceRegistration.hubLacksRegistration(HubClientError.server(status: 405, message: "")))
        XCTAssertTrue(DeviceRegistration.hubLacksRegistration(HubClientError.notFound("")))
        XCTAssertFalse(DeviceRegistration.hubLacksRegistration(HubClientError.forbidden("admin")))
        XCTAssertFalse(DeviceRegistration.hubLacksRegistration(HubClientError.unauthorized("")))
        XCTAssertFalse(DeviceRegistration.hubLacksRegistration(HubClientError.server(status: 500, message: "")))
        XCTAssertFalse(DeviceRegistration.hubLacksRegistration(HubClientError.transport("offline")))
    }

    func testRegisterAndRevokeRequests() async throws {
        let session = StubProtocol.session { request in
            switch (request.httpMethod, request.url?.path) {
            case ("POST"?, "/v1/devices"?):
                return (200, #"{"device":{"id":"d1","name":"Mac","platform":"macos","online":false,"capabilities":[],"last_seen":null},"token":"d1.s"}"#)
            case ("DELETE"?, "/v1/devices/d1"?):
                return (200, #"{"revoked":"d1"}"#)
            case ("DELETE"?, _):
                return (404, #"{"error":"no such device"}"#)
            default:
                return (405, "")
            }
        }
        let client = try HubClient(baseURL: "http://hub.test", token: "admin.token", session: session)
        let registered = try await client.registerDevice(name: "Mac", platform: "macos")
        XCTAssertEqual(registered.token, "d1.s")
        XCTAssertEqual(registered.device.id, "d1")
        let post = try XCTUnwrap(StubProtocol.requests.first { $0.method == "POST" })
        XCTAssertEqual(post.authorization, "Bearer admin.token")
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: post.body) as? [String: String])
        XCTAssertEqual(body, ["name": "Mac", "platform": "macos"])

        let revoked = try await client.revokeDevice(id: "d1")
        XCTAssertEqual(revoked.revoked, "d1")
        do {
            try await client.revokeDevice(id: "gone")
            XCTFail("expected 404")
        } catch {
            XCTAssertEqual(error as? HubClientError, .notFound("no such device"))
        }
    }
}

/// Answers requests from a closure, and records them (method, path, Authorization, body).
private final class StubProtocol: URLProtocol, @unchecked Sendable {
    struct Seen { let method: String; let path: String; let authorization: String?; let body: Data }

    nonisolated(unsafe) static var handler: ((URLRequest) -> (Int, String))?
    nonisolated(unsafe) static var requests: [Seen] = []

    static func session(_ handler: @escaping (URLRequest) -> (Int, String)) -> URLSession {
        self.handler = handler
        requests = []
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubProtocol.self]
        return URLSession(configuration: configuration)
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let body = request.httpBody ?? request.httpBodyStream.map(Self.read) ?? Data()
        Self.requests.append(Seen(method: request.httpMethod ?? "GET", path: request.url?.path ?? "",
                                  authorization: request.value(forHTTPHeaderField: "Authorization"), body: body))
        let (status, text) = Self.handler?(request) ?? (500, "")
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1",
                                       headerFields: ["Content-Type": "application/json"])!
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
