import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
@testable import GrouperAPI

final class GoogleSignInTests: XCTestCase {
    private let clientID = "1234-abcdef.apps.googleusercontent.com"

    // MARK: Client ID and redirect

    func testClientIDAndRedirect() throws {
        let oauth = try XCTUnwrap(GoogleOAuth(clientID: " \(clientID)\n"))
        XCTAssertEqual(oauth.clientID, clientID)
        XCTAssertEqual(oauth.redirectScheme, "com.googleusercontent.apps.1234-abcdef")
        XCTAssertEqual(oauth.redirectURI, "com.googleusercontent.apps.1234-abcdef:/oauth2redirect")
        XCTAssertNil(GoogleOAuth(clientID: ""))
        XCTAssertNil(GoogleOAuth(clientID: ".apps.googleusercontent.com"))
        XCTAssertNil(GoogleOAuth(clientID: "1234-abcdef"))
        XCTAssertNil(GoogleOAuth(clientID: "12 34.apps.googleusercontent.com"))
    }

    // MARK: PKCE

    func testSHA256KnownVectors() {
        func hex(_ text: String) -> String { SHA256Digest.hash(Array(text.utf8)).map { String(format: "%02x", $0) }.joined() }
        XCTAssertEqual(hex(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        XCTAssertEqual(hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        // Two blocks after padding.
        XCTAssertEqual(hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
                       "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1")
        // Exactly 64 bytes: the padding needs its own block.
        XCTAssertEqual(hex(String(repeating: "a", count: 64)),
                       "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb")
    }

    func testCodeChallengeIsS256OfTheVerifier() {
        // RFC 7636, appendix B.
        XCTAssertEqual(GoogleOAuth.codeChallenge(for: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
                       "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")
    }

    func testRandomVerifiers() {
        let allowed = Set("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")
        let first = GoogleOAuth.randomVerifier()
        let second = GoogleOAuth.randomVerifier()
        XCTAssertEqual(first.count, 64)
        XCTAssertTrue(first.allSatisfy { allowed.contains($0) })
        XCTAssertNotEqual(first, second)
    }

    // MARK: Authorization URL

    func testAuthorizationURL() throws {
        let oauth = try XCTUnwrap(GoogleOAuth(clientID: clientID))
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        let request = oauth.authorizationRequest(nonce: "n0nce", state: "st4te", codeVerifier: verifier)
        XCTAssertEqual(request.state, "st4te")
        XCTAssertEqual(request.codeVerifier, verifier)
        XCTAssertEqual(request.nonce, "n0nce")

        let components = try XCTUnwrap(URLComponents(url: request.url, resolvingAgainstBaseURL: false))
        XCTAssertEqual(components.scheme, "https")
        XCTAssertEqual(components.host, "accounts.google.com")
        XCTAssertEqual(components.path, "/o/oauth2/v2/auth")
        var query: [String: String] = [:]
        for item in components.queryItems ?? [] { query[item.name] = item.value }
        XCTAssertEqual(query, [
            "client_id": clientID,
            "redirect_uri": "com.googleusercontent.apps.1234-abcdef:/oauth2redirect",
            "response_type": "code",
            "scope": "openid email profile",
            "state": "st4te",
            "nonce": "n0nce",
            "code_challenge": "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            "code_challenge_method": "S256",
            "prompt": "select_account",
        ])
        // Spaces and the redirect's ':' and '/' are percent-encoded on the wire.
        let raw = try XCTUnwrap(components.percentEncodedQuery)
        XCTAssertTrue(raw.contains("scope=openid%20email%20profile"))
        XCTAssertTrue(raw.contains("redirect_uri=com.googleusercontent.apps.1234-abcdef%3A%2Foauth2redirect"))
    }

    func testFreshStateAndVerifierPerRequest() throws {
        let oauth = try XCTUnwrap(GoogleOAuth(clientID: clientID))
        let one = oauth.authorizationRequest(nonce: "n")
        let two = oauth.authorizationRequest(nonce: "n")
        XCTAssertNotEqual(one.state, two.state)
        XCTAssertNotEqual(one.codeVerifier, two.codeVerifier)
        XCTAssertEqual(one.codeVerifier.count, 64)
    }

    // MARK: Callback

    func testCallbackParsing() throws {
        let oauth = try XCTUnwrap(GoogleOAuth(clientID: clientID))
        let request = oauth.authorizationRequest(nonce: "n", state: "good", codeVerifier: "v")
        let scheme = "com.googleusercontent.apps.1234-abcdef"

        let ok = try XCTUnwrap(URL(string: "\(scheme):/oauth2redirect?state=good&code=4%2F0AbCd&scope=email%20openid"))
        XCTAssertEqual(try oauth.authorizationCode(from: ok, for: request), "4/0AbCd")

        let denied = try XCTUnwrap(URL(string: "\(scheme):/oauth2redirect?error=access_denied&state=good"))
        XCTAssertThrowsError(try oauth.authorizationCode(from: denied, for: request)) {
            XCTAssertEqual($0 as? GoogleOAuthError, .google("access_denied"))
        }

        let otherState = try XCTUnwrap(URL(string: "\(scheme):/oauth2redirect?state=evil&code=abc"))
        XCTAssertThrowsError(try oauth.authorizationCode(from: otherState, for: request)) {
            XCTAssertEqual($0 as? GoogleOAuthError, .stateMismatch)
        }

        let noState = try XCTUnwrap(URL(string: "\(scheme):/oauth2redirect?code=abc"))
        XCTAssertThrowsError(try oauth.authorizationCode(from: noState, for: request)) {
            XCTAssertEqual($0 as? GoogleOAuthError, .stateMismatch)
        }

        let noCode = try XCTUnwrap(URL(string: "\(scheme):/oauth2redirect?state=good"))
        XCTAssertThrowsError(try oauth.authorizationCode(from: noCode, for: request)) {
            XCTAssertEqual($0 as? GoogleOAuthError, .missingCode)
        }

        let otherScheme = try XCTUnwrap(URL(string: "grouper://oauth2redirect?state=good&code=abc"))
        XCTAssertThrowsError(try oauth.authorizationCode(from: otherScheme, for: request)) {
            XCTAssertEqual($0 as? GoogleOAuthError, .wrongCallback)
        }
    }

    // MARK: Token exchange

    func testTokenResponseDecoding() throws {
        let ok = #"{"access_token":"ya29.x","expires_in":3599,"id_token":"eyJ.a.b","scope":"openid email profile","token_type":"Bearer"}"#
        XCTAssertEqual(try GoogleOAuth.idToken(status: 200, data: Data(ok.utf8)), "eyJ.a.b")

        let refused = #"{"error":"invalid_grant","error_description":"Bad Request"}"#
        XCTAssertThrowsError(try GoogleOAuth.idToken(status: 400, data: Data(refused.utf8))) {
            XCTAssertEqual($0 as? GoogleOAuthError, .tokenExchange("invalid_grant: Bad Request"))
        }
        XCTAssertThrowsError(try GoogleOAuth.idToken(status: 502, data: Data("<html>".utf8))) {
            XCTAssertEqual($0 as? GoogleOAuthError, .tokenExchange("HTTP 502"))
        }
        XCTAssertThrowsError(try GoogleOAuth.idToken(status: 200, data: Data(#"{"access_token":"ya29.x"}"#.utf8))) {
            XCTAssertEqual($0 as? GoogleOAuthError, .missingIDToken)
        }
    }

    func testExchangePostsTheFormWithoutASecret() async throws {
        let session = GoogleStub.session { request in
            guard request.url?.absoluteString == "https://oauth2.googleapis.com/token" else { return (404, "") }
            return (200, #"{"access_token":"ya29.x","id_token":"eyJ.id.token","token_type":"Bearer"}"#)
        }
        let oauth = try XCTUnwrap(GoogleOAuth(clientID: clientID))
        let idToken = try await oauth.exchange(code: "4/0AbCd", codeVerifier: "verifier-1", session: session)
        XCTAssertEqual(idToken, "eyJ.id.token")

        let seen = try XCTUnwrap(GoogleStub.requests.first)
        XCTAssertEqual(seen.method, "POST")
        XCTAssertEqual(seen.contentType, "application/x-www-form-urlencoded")
        XCTAssertNil(seen.authorization)
        let form = Self.form(seen.body)
        XCTAssertEqual(form, [
            "code": "4/0AbCd",
            "client_id": clientID,
            "code_verifier": "verifier-1",
            "redirect_uri": "com.googleusercontent.apps.1234-abcdef:/oauth2redirect",
            "grant_type": "authorization_code",
        ])
        XCTAssertNil(form["client_secret"])
        XCTAssertTrue(String(decoding: seen.body, as: UTF8.self).contains("code=4%2F0AbCd"))
    }

    func testExchangeReportsGooglesError() async throws {
        let session = GoogleStub.session { _ in (400, #"{"error":"invalid_grant","error_description":"Malformed auth code."}"#) }
        let oauth = try XCTUnwrap(GoogleOAuth(clientID: clientID))
        do {
            _ = try await oauth.exchange(code: "x", codeVerifier: "v", session: session)
            XCTFail("expected an error")
        } catch {
            XCTAssertEqual(error as? GoogleOAuthError, .tokenExchange("invalid_grant: Malformed auth code."))
        }
    }

    // MARK: Hub endpoints

    func testHubSignInCalls() async throws {
        let session = GoogleStub.session { request in
            switch (request.httpMethod, request.url?.path) {
            case ("GET"?, "/v1/auth/status"?):
                return (200, #"{"claimed":false,"google":true,"google_web":false}"#)
            case ("POST"?, "/v1/auth/google/nonce"?):
                return (200, #"{"nonce":"abc123","expires":1795000000}"#)
            case ("POST"?, "/v1/auth/google"?):
                return (200, #"{"token":"dev1.secret","device":{"id":"dev1","name":"Grouper on iPhone","platform":"ios","admin":true}}"#)
            default:
                return (404, #"{"error":"not found"}"#)
            }
        }
        let hub = try HubClient(baseURL: "http://hub.test", session: session)

        let status = try await hub.authStatus()
        XCTAssertEqual(status, AuthStatus(claimed: false, google: true, googleWeb: false))
        let nonce = try await hub.googleNonce()
        XCTAssertEqual(nonce, GoogleNonce(nonce: "abc123", expires: 1_795_000_000))
        let result = try await hub.signInWithGoogle(idToken: "eyJ.id.token", setupCode: " K7PM-3XQR-T9WD ",
                                                    client: .ios, deviceName: "Grouper on iPhone")
        XCTAssertEqual(result.token, "dev1.secret")
        XCTAssertEqual(result.device.id, "dev1")
        XCTAssertTrue(result.device.admin)
        XCTAssertEqual(result.device.platform, "ios")

        XCTAssertTrue(GoogleStub.requests.allSatisfy { $0.authorization == nil }, "signed-out calls send no token")
        let post = try XCTUnwrap(GoogleStub.requests.first { $0.path == "/v1/auth/google" })
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: post.body) as? [String: String])
        XCTAssertEqual(body, ["id_token": "eyJ.id.token", "setup_code": "K7PM-3XQR-T9WD", "client": "ios",
                              "device_name": "Grouper on iPhone"])
    }

    func testSignInLeavesOutAnEmptySetupCodeAndMapsRefusals() async throws {
        let session = GoogleStub.session { _ in (403, #"{"error":"this hub belongs to a different Google account"}"#) }
        let hub = try HubClient(baseURL: "http://hub.test", session: session)
        do {
            _ = try await hub.signInWithGoogle(idToken: "t", setupCode: "  ", client: .macos)
            XCTFail("expected 403")
        } catch {
            XCTAssertEqual(error as? HubClientError, .forbidden("this hub belongs to a different Google account"))
        }
        let post = try XCTUnwrap(GoogleStub.requests.first)
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: post.body) as? [String: String])
        XCTAssertEqual(body, ["id_token": "t", "client": "macos"])
    }

    func testLinkCalls() async throws {
        let owner = #"{"provider":"google","email":"owner@example.com","name":"Owner","claimed_at":1794000000}"#
        let session = GoogleStub.session { request in
            switch request.httpMethod {
            case "GET"?: return (200, #"{"owner":null}"#)
            case "POST"?: return (200, "{\"owner\":\(owner)}")
            case "DELETE"?: return (200, #"{"unlinked":true}"#)
            default: return (405, "")
            }
        }
        let hub = try HubClient(baseURL: "http://hub.test/", token: "admin.token", session: session)
        let none = try await hub.linkedGoogleAccount()
        XCTAssertNil(none)
        let linked = try await hub.linkGoogle(idToken: "eyJ.id.token")
        XCTAssertEqual(linked, LinkedGoogleAccount(email: "owner@example.com", name: "Owner", claimedAt: 1_794_000_000))
        let unlinked = try await hub.unlinkGoogle()
        XCTAssertTrue(unlinked)

        XCTAssertEqual(GoogleStub.requests.map(\.path), Array(repeating: "/v1/auth/google/link", count: 3))
        XCTAssertTrue(GoogleStub.requests.allSatisfy { $0.authorization == "Bearer admin.token" })
        let post = try XCTUnwrap(GoogleStub.requests.first { $0.method == "POST" })
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: post.body) as? [String: String])
        XCTAssertEqual(body, ["id_token": "eyJ.id.token"])
    }

    // MARK: Setup codes

    func testSetupCodes() {
        XCTAssertEqual(SetupCode.normalized("K7PM-3XQR-T9WD"), "K7PM-3XQR-T9WD")
        XCTAssertEqual(SetupCode.normalized(" k7pm 3xqr t9wd "), "K7PM-3XQR-T9WD")
        XCTAssertEqual(SetupCode.normalized("K7PM3XQRT9WD"), "K7PM-3XQR-T9WD")
        XCTAssertNil(SetupCode.normalized("K7PM-3XQR-T9W"))
        XCTAssertNil(SetupCode.normalized("K7PM-3XQR-T9WDX"))
        XCTAssertNil(SetupCode.normalized("O7PM-3XQR-T9WD"), "no O, 0, 1, I or L")
        XCTAssertNil(SetupCode.normalized(""))
    }

    private static func form(_ body: Data) -> [String: String] {
        var result: [String: String] = [:]
        for pair in String(decoding: body, as: UTF8.self).split(separator: "&") {
            let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
            guard parts.count == 2 else { continue }
            result[parts[0]] = parts[1].removingPercentEncoding
        }
        return result
    }
}

/// Answers requests from a closure, and records them.
private final class GoogleStub: URLProtocol, @unchecked Sendable {
    struct Seen { let method: String; let path: String; let authorization: String?; let contentType: String?; let body: Data }

    nonisolated(unsafe) static var handler: ((URLRequest) -> (Int, String))?
    nonisolated(unsafe) static var requests: [Seen] = []

    static func session(_ handler: @escaping (URLRequest) -> (Int, String)) -> URLSession {
        self.handler = handler
        requests = []
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [GoogleStub.self]
        return URLSession(configuration: configuration)
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let body = request.httpBody ?? request.httpBodyStream.map(Self.read) ?? Data()
        Self.requests.append(Seen(method: request.httpMethod ?? "GET", path: request.url?.path ?? "",
                                  authorization: request.value(forHTTPHeaderField: "Authorization"),
                                  contentType: request.value(forHTTPHeaderField: "Content-Type"), body: body))
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
