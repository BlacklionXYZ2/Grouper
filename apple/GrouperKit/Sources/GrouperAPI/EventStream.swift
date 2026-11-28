import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

public enum EventStreamState: Equatable, Sendable {
    case connecting(attempt: Int)
    case connected
    case disconnected
}

/// Stored records and live-only deltas are deliberately separate so callers never cache `seq == 0`.
public enum EventStreamUpdate: Equatable, Sendable {
    case state(EventStreamState)
    case stored(Event)
    case live(Event)
}

public final class EventStream: @unchecked Sendable {
    public let baseURL: URL
    public let conversationId: String?
    private let token: String
    /// Sent as `Grouper-Relay` on the WebSocket handshake and the catch-up calls (a relay address).
    private let relayToken: String?
    private let session: URLSession
    private let socketLock = NSLock()
    private var currentSocket: URLSessionWebSocketTask?

    public init(baseURL: URL, token: String, conversationId: String? = nil, relayToken: String? = nil, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.token = token
        self.relayToken = relayToken.flatMap { $0.isEmpty ? nil : $0 }
        self.conversationId = conversationId
        self.session = session
    }

    /// Follows `/v1/events` until cancelled. Every reconnect opens the socket first,
    /// catches up over HTTP by the last durable seq, then drains socket frames with
    /// duplicate stored events discarded. Live deltas are forwarded but never cached.
    public func updates(after initialSequence: Int64 = 0) -> AsyncThrowingStream<EventStreamUpdate, Error> {
        AsyncThrowingStream { continuation in
            let task = Task {
                await self.run(after: initialSequence, continuation: continuation)
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// Closes the current socket without ending `updates`. The stream reconnects,
    /// catches up durable events over HTTP, and then resumes live delivery.
    ///
    /// This is also useful when an app moves between networks and wants to replace
    /// a socket immediately instead of waiting for the next receive to fail.
    @discardableResult
    public func forceReconnect() -> Bool {
        socketLock.lock()
        let socket = currentSocket
        socketLock.unlock()
        socket?.cancel(with: .goingAway, reason: nil)
        return socket != nil
    }

    private func run(after initialSequence: Int64, continuation: AsyncThrowingStream<EventStreamUpdate, Error>.Continuation) async {
        var lastSequence = initialSequence
        var attempt = 0
        while !Task.isCancelled {
            continuation.yield(.state(.connecting(attempt: attempt)))
            var socket: URLSessionWebSocketTask?
            do {
                let client = try HubClient(baseURL: baseURL, token: token, relayToken: relayToken, session: session)
                let request = handshakeRequest(url: try websocketURL())
                let opened = session.webSocketTask(with: request)
                socket = opened
                setCurrentSocket(opened)
                opened.resume()

                // The server chooses its socket cursor at upgrade time. Fetching only
                // after opening is what closes the reconnect race; later frames queue.
                let caughtUp: [Event]
                if let conversationId {
                    caughtUp = try await client.events(conversationId: conversationId, after: lastSequence)
                } else {
                    // There is no account-wide HTTP event route. Consumers that follow
                    // all conversations catch each loaded conversation up separately.
                    caughtUp = []
                }
                for event in caughtUp where event.seq > lastSequence {
                    lastSequence = event.seq
                    continuation.yield(.stored(event))
                }
                attempt = 0
                continuation.yield(.state(.connected))

                while !Task.isCancelled {
                    let message = try await opened.receive()
                    let data: Data
                    switch message {
                    case .data(let received): data = received
                    case .string(let received): data = Data(received.utf8)
                    @unknown default: continue
                    }
                    let event: Event
                    do { event = try JSONDecoder.grouper.decode(Event.self, from: data) }
                    catch { throw HubClientError.decoding(String(describing: error)) }
                    if event.seq == 0 {
                        continuation.yield(.live(event))
                    } else if event.seq > lastSequence {
                        lastSequence = event.seq
                        continuation.yield(.stored(event))
                    }
                }
                opened.cancel(with: .goingAway, reason: nil)
                clearCurrentSocket(opened)
            } catch is CancellationError {
                socket?.cancel(with: .goingAway, reason: nil)
                clearCurrentSocket(socket)
                break
            } catch let error as HubClientError {
                socket?.cancel(with: .goingAway, reason: nil)
                clearCurrentSocket(socket)
                if case .unauthorized = error { continuation.finish(throwing: error); return }
                if case .forbidden = error { continuation.finish(throwing: error); return }
                // The relay sign-in expired: reconnecting can't fix that, the app must sign in again.
                if case .relayUnauthorized = error { continuation.finish(throwing: error); return }
            } catch {
                socket?.cancel(with: .goingAway, reason: nil)
                clearCurrentSocket(socket)
            }
            continuation.yield(.state(.disconnected))
            attempt += 1
            let seconds = min(15.0, pow(2.0, Double(max(0, attempt - 1))))
            do { try await Task.sleep(for: .seconds(seconds)) }
            catch { break }
        }
        continuation.yield(.state(.disconnected))
        continuation.finish()
    }

    /// The WebSocket upgrade request: the device token, plus the relay token for a relay address.
    func handshakeRequest(url: URL) -> URLRequest {
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        if let relayToken { request.setValue(relayToken, forHTTPHeaderField: RelayHeader.name) }
        return request
    }

    private func setCurrentSocket(_ socket: URLSessionWebSocketTask) {
        socketLock.lock()
        currentSocket = socket
        socketLock.unlock()
    }

    private func clearCurrentSocket(_ socket: URLSessionWebSocketTask?) {
        socketLock.lock()
        if let socket, currentSocket === socket {
            currentSocket = nil
        }
        socketLock.unlock()
    }

    private func websocketURL() throws -> URL {
        guard var components = URLComponents(url: baseURL, resolvingAgainstBaseURL: false) else { throw HubClientError.invalidBaseURL }
        switch components.scheme?.lowercased() {
        case "https": components.scheme = "wss"
        case "http": components.scheme = "ws"
        default: throw HubClientError.invalidBaseURL
        }
        components.path = components.path.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        components.path = "/" + ([components.path].filter { !$0.isEmpty } + ["v1", "events"]).joined(separator: "/")
        if let conversationId { components.queryItems = [URLQueryItem(name: "conversation", value: conversationId)] }
        guard let url = components.url else { throw HubClientError.invalidBaseURL }
        return url
    }
}
