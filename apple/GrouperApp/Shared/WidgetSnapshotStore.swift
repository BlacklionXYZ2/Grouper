import Foundation
import GrouperAPI
#if canImport(WidgetKit)
import WidgetKit
#endif

/// Where the widgets' snapshot lives: one JSON file in the App Group container, written by the app
/// (`UsageStore`) and read by the widget extension. Compiled into both targets.
///
/// The widget never signs in and never sees a token: it only reads what the Usage page already
/// loaded. If the App Group is not available (an unsigned or unprovisioned build), there is no
/// container, nothing is written, and the widgets say "Open Grouper to load usage".
enum WidgetSnapshotStore {
    /// `group.` + the app's bundle id prefix (`uk.clockmillbarn.grouper`). Must match both targets'
    /// entitlements in `apple/GrouperApp/project.yml` and the identifier registered on the Apple
    /// developer account.
    static let appGroup = "group.uk.clockmillbarn.grouper"
    private static let fileName = "usage-widgets.json"

    static var fileURL: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup)?
            .appendingPathComponent(fileName)
    }

    static func load() -> WidgetSnapshot? {
        guard let url = fileURL, let data = try? Data(contentsOf: url) else { return nil }
        return WidgetSnapshot.decode(data)
    }

    /// Returns false when there is nowhere to write or the write failed.
    @discardableResult
    static func save(_ snapshot: WidgetSnapshot) -> Bool {
        guard let url = fileURL, let data = snapshot.encoded() else { return false }
        do {
            try data.write(to: url, options: [.atomic])
            return true
        } catch {
            return false
        }
    }

    /// Signing out removes the copy (it holds spend figures) and blanks the widgets.
    static func clear() {
        if let url = fileURL { try? FileManager.default.removeItem(at: url) }
        reloadTimelines()
    }

    static func reloadTimelines() {
        #if canImport(WidgetKit)
        WidgetCenter.shared.reloadAllTimelines()
        #endif
    }
}
