import Foundation
import XCTest
@testable import GrouperAPI

final class HubAddressTests: XCTestCase {
    func testDefaultAddress() {
        XCTAssertEqual(HubAddress.defaultAddress, "https://grouper.clockmillbarn.uk")
        XCTAssertEqual(try HubAddress.validate(HubAddress.defaultAddress), HubAddress.defaultAddress)
        XCTAssertTrue(HubAddress.isDefault("https://grouper.clockmillbarn.uk"))
        XCTAssertTrue(HubAddress.isDefault("  HTTPS://GROUPER.CLOCKMILLBARN.UK/  "))
        XCTAssertTrue(HubAddress.isDefault("https://grouper.clockmillbarn.uk:443"))
        XCTAssertFalse(HubAddress.isDefault("https://other.example"))
        XCTAssertFalse(HubAddress.isDefault("http://grouper.clockmillbarn.uk"))
    }

    func testResolutionOrder() {
        // No override -> built-in default.
        XCTAssertEqual(HubAddress.resolve(savedOverride: nil), HubAddress.defaultAddress)
        XCTAssertEqual(HubAddress.resolve(savedOverride: ""), HubAddress.defaultAddress)
        XCTAssertEqual(HubAddress.resolve(savedOverride: "   "), HubAddress.defaultAddress)

        // Valid saved override wins over default.
        XCTAssertEqual(
            HubAddress.resolve(savedOverride: " https://My-Hub.Example/ "),
            "https://my-hub.example"
        )
        XCTAssertEqual(
            HubAddress.resolve(savedOverride: "http://192.168.1.42:47600/"),
            "http://192.168.1.42:47600"
        )
        XCTAssertEqual(
            HubAddress.resolve(savedOverride: "http://localhost:47600"),
            "http://localhost:47600"
        )

        // Invalid saved override falls back to default rather than stranding the app.
        XCTAssertEqual(HubAddress.resolve(savedOverride: "http://remote.example"), HubAddress.defaultAddress)
        XCTAssertEqual(HubAddress.resolve(savedOverride: "not a url"), HubAddress.defaultAddress)
        XCTAssertEqual(HubAddress.resolve(savedOverride: "ftp://grouper.clockmillbarn.uk"), HubAddress.defaultAddress)
    }

    func testHTTPSValidationAndNormalization() throws {
        XCTAssertEqual(try HubAddress.validate("https://grouper.example"), "https://grouper.example")
        XCTAssertEqual(try HubAddress.validate("  HTTPS://Grouper.Example/// \n"), "https://grouper.example")
        XCTAssertEqual(try HubAddress.validate("https://grouper.example:8443/"), "https://grouper.example:8443")
        XCTAssertEqual(try HubAddress.validate("https://grouper.example:443/"), "https://grouper.example")
        XCTAssertEqual(try HubAddress.validate("https://grouper.example/hub/"), "https://grouper.example/hub")
        XCTAssertEqual(try HubAddress.validate("https://192.168.1.5:47600"), "https://192.168.1.5:47600")
        XCTAssertEqual(try HubAddress.validate("https://8.8.8.8"), "https://8.8.8.8")
        XCTAssertEqual(try HubAddress.validate("https://[::1]:8443/"), "https://[::1]:8443")
    }

    func testHTTPAllowedForLocalhostAndLAN() throws {
        let allowed = [
            ("http://localhost", "http://localhost"),
            ("http://LOCALHOST:47600/", "http://localhost:47600"),
            ("http://localhost:80", "http://localhost"),
            ("http://hub.localhost:47600", "http://hub.localhost:47600"),
            ("http://oscars-pc.local:47600/", "http://oscars-pc.local:47600"),
            ("http://127.0.0.1:47600", "http://127.0.0.1:47600"),
            ("http://127.255.0.1:8080", "http://127.255.0.1:8080"),
            ("http://10.0.0.1:47600", "http://10.0.0.1:47600"),
            ("http://10.255.255.254", "http://10.255.255.254"),
            ("http://172.16.0.1:47600", "http://172.16.0.1:47600"),
            ("http://172.31.255.254:47600", "http://172.31.255.254:47600"),
            ("http://192.168.0.1:47600", "http://192.168.0.1:47600"),
            ("http://192.168.1.100", "http://192.168.1.100"),
            ("http://169.254.1.2:47600", "http://169.254.1.2:47600"),
            ("http://[::1]:47600/", "http://[::1]:47600"),
            ("http://[FD12:3456:789A::1]:47600", "http://[fd12:3456:789a::1]:47600"),
            ("http://[fc00::1]:47600", "http://[fc00::1]:47600"),
            ("http://[fe80::1]:47600", "http://[fe80::1]:47600"),
            ("http://[::ffff:192.168.1.5]:47600", "http://[::ffff:192.168.1.5]:47600"),
            ("http://[::ffff:127.0.0.1]:47600", "http://[::ffff:127.0.0.1]:47600"),
        ]
        for (input, expected) in allowed {
            XCTAssertEqual(try HubAddress.validate(input), expected, "expected \(input) to be accepted")
        }
    }

    func testHTTPRejectedForRemoteHosts() {
        let rejected = [
            "http://grouper.clockmillbarn.uk",
            "http://example.com",
            "http://example.com:47600",
            "http://localhost.example.com",
            "http://my.local.example.com",
            "http://.local",
            "http://..local",
            "http://local",
            "http://8.8.8.8",
            "http://1.1.1.1:47600",
            "http://0.0.0.0:47600",
            "http://11.0.0.1",
            "http://172.15.255.255",
            "http://172.32.0.1",
            "http://192.169.0.1",
            "http://169.253.0.1",
            "http://100.64.0.1",
            "http://0177.0.0.1",
            "http://[2001:db8::1]:47600",
            "http://[::]:47600",
            "http://[::ffff:8.8.8.8]:47600",
        ]
        for input in rejected {
            XCTAssertThrowsError(try HubAddress.validate(input), "expected \(input) to be rejected") { error in
                XCTAssertEqual(error as? HubAddressError, .insecureRemoteHost, "unexpected error for \(input): \(error)")
            }
        }
    }

    func testInvalidAndUnsupportedAddresses() {
        XCTAssertThrowsError(try HubAddress.validate("")) {
            XCTAssertEqual($0 as? HubAddressError, .empty)
        }
        XCTAssertThrowsError(try HubAddress.validate("   \n\t ")) {
            XCTAssertEqual($0 as? HubAddressError, .empty)
        }
        for badScheme in ["grouper.clockmillbarn.uk", "localhost:47600", "ftp://grouper.clockmillbarn.uk", "ws://localhost:47600", "javascript:alert(1)"] {
            XCTAssertThrowsError(try HubAddress.validate(badScheme), "expected unsupported scheme for \(badScheme)") {
                XCTAssertEqual($0 as? HubAddressError, .unsupportedScheme)
            }
        }
        for malformed in [
            "https://",
            "https:///path",
            "https://user:pass@grouper.clockmillbarn.uk",
            "https://user@grouper.clockmillbarn.uk",
            "https://grouper.clockmillbarn.uk?foo=bar",
            "https://grouper.clockmillbarn.uk#frag",
            "https://grouper.clockmillbarn.uk:0",
            "https://grouper.clockmillbarn.uk:abc",
            "https://grouper clockmillbarn.uk",
        ] {
            XCTAssertThrowsError(try HubAddress.validate(malformed), "expected invalidURL for \(malformed)") {
                XCTAssertEqual($0 as? HubAddressError, .invalidURL)
            }
        }
    }

    func testDisplayHost() {
        XCTAssertEqual(HubAddress.displayHost("https://grouper.clockmillbarn.uk"), "grouper.clockmillbarn.uk")
        XCTAssertEqual(HubAddress.displayHost("https://grouper.clockmillbarn.uk:443/"), "grouper.clockmillbarn.uk")
        XCTAssertEqual(HubAddress.displayHost("https://grouper.example:8443"), "grouper.example:8443")
        XCTAssertEqual(HubAddress.displayHost("http://localhost:47600"), "localhost:47600")
        XCTAssertEqual(HubAddress.displayHost("http://localhost:80"), "localhost")
        XCTAssertEqual(HubAddress.displayHost("http://[::1]:47600"), "[::1]:47600")
    }

    func testValidatedAddressAcceptedByHubClient() throws {
        for raw in [
            HubAddress.defaultAddress,
            "http://localhost:47600",
            "http://192.168.1.10:47600",
            "http://[::1]:47600",
        ] {
            let valid = try HubAddress.validate(raw)
            let client = try HubClient(baseURL: valid)
            XCTAssertNotNil(client.baseURL.host)
        }
    }
}
