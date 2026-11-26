# Grouper apps

The iPhone and Mac apps share the Foundation-only `GrouperAPI` package and the
SwiftUI sources in `Shared/`. The Mac target also links `GrouperKit` for its local
device node. Tokens are stored in the Keychain as `grouper.admin` and
`grouper.device` (with the device's id in `grouper.device.id`); they are never written
to defaults or logs.

## Sign in with Google

`docs/accounts-design.md` has the design. The sign-in screen asks for the hub address,
then checks `GET /v1/auth/status`. When the hub has Google sign-in on and the app has a
client ID, it shows **Sign in with Google**, with the admin-token field under **Use a token
instead**. An unclaimed hub first asks for its setup code (`XXXX-XXXX-XXXX`, from the hub's
log or `grouper-hub setup-code`). The app gets a nonce from the hub
(`POST /v1/auth/google/nonce`) and opens Google in `ASWebAuthenticationSession`, using an
authorization code with PKCE (S256) and no client secret. It exchanges the code at Google's
token endpoint for an ID token, then sends that to `POST /v1/auth/google` with `client`
`ios` or `macos` and the device name "Grouper on <device name>". The hub answers with an
admin token for this install. The app keeps it in the Keychain as `grouper.admin`, the
same as a pasted token, and connects the same way. On the Mac, connecting also registers
the Mac's node device. Settings > Account shows the linked Google account with **Unlink**,
or offers **Link your Google account** on a hub that has a token owner but no Google account.

The URL building, PKCE, callback checks, token exchange and hub calls live in
`GrouperAPI` (`GoogleOAuth`, `HubClient.signInWithGoogle` and related calls) and have unit
tests. The browser step is `Shared/GoogleWebSignIn.swift`.

### Client IDs and URL schemes

Each app has its own Google OAuth client ID. Both are Google's **iOS** client type: one for
bundle ID `uk.clockmillbarn.grouper.ios` and one for `uk.clockmillbarn.grouper.macos`. Client
IDs are public identifiers, not secrets. Grouper's own clients (Google Cloud project
`grouper-515912`) are set in each app's Info.plist (`iOS/Info.plist`, `macOS/Info.plist`); a
fork changes them there:

1. `GrouperGoogleClientID`: the full client ID, e.g.
   `1234-abcd.apps.googleusercontent.com`. If it's empty or isn't a Google client ID, the
   app hides Google sign-in.
2. Under `CFBundleURLTypes`, the entry named `uk.clockmillbarn.grouper.google`: replace the
   placeholder scheme `com.googleusercontent.apps.REPLACE-WITH-…` with the reversed client
   ID, e.g. `com.googleusercontent.apps.1234-abcd`. Google sends the sign-in back to
   `<that scheme>:/oauth2redirect`, and the app derives the same scheme from the client ID.

The hub must also trust both client IDs: they are its defaults once they're filled in
`crates/hub/src/google_auth.rs`, or set them with `GROUPER_GOOGLE_CLIENT_IDS`.

The "G" on the button is drawn in SwiftUI (`GoogleGMark`). Swap it for Google's official
asset from their branding guidelines once the app has an asset catalogue.

## This Mac as a device

Sign-in gets an admin token, either pasted or from Google sign-in. The admin token powers the
window; the local node never gets it. On Connect, and at launch with saved
credentials, the Mac checks its saved device token. With none (or one the hub rejects
with 401) it registers itself through `POST /v1/devices` under the Mac's computer
name with platform `macos`, keeps the new token in the Keychain and starts the node. A
rejected token's old device is revoked, best effort. The node also reports a 401 while
running; the app confirms it with the hub and registers again.

Settings > This Mac shows the name and device id it is registered as, **Register
again** (a new device, then the old one revoked), and **Use an existing device token**
for a plain token made with `grouper-hub device add`. A hub too old for
`POST /v1/devices` (404 or 405) gets a plain message and the manual field opens.
**Sign out on this device** keeps the device token, so the node keeps serving;
**Sign out and remove this Mac** revokes it on the hub first. The iPhone app runs no
node and has no device token.

## Status: GUI decisions of 25 November (unbuilt)

The SwiftUI sources follow the owner decisions in `docs/gui-spec.md`, with the web
client as the reference. **They have not been compiled or run.** Treat them as
unverified until they build in Xcode 26 and are checked on an iPhone and a Mac.
Only the `GrouperAPI` parts (model family rules, the thread-as-chat fold, elapsed
time, `ProviderInfo.viaDevice`) are built and tested, on the Windows toolchain.

- **No Threads tab or Threads destination.** Threads belong to their chat. The
  phone keeps Chat, Devices and More (the tab set is still an owner decision; see
  the `TODO(owner decision: phone tabs)` in `iOS/GrouperIOSApp.swift`). The Chat
  tab carries the needs-you badge.
- **Threads button in the chat toolbar** (count, orange dot when something needs
  you) opens the chat's threads: a sheet on iPhone, an inspector on Mac
  (`Shared/ThreadsPanel.swift`). Rows show the title, live elapsed time, the model
  name with a family colour and glyph, the device and status, grouped as Needs
  you / Running / Finished.
- **Thread view reads like a chat** (`Shared/ThreadDetailView.swift`): the brief
  first, the model's text labelled with the model, collapsible tool rows with the
  exact input and output, step-ins and orchestrator answers as messages, a fixed
  model chip in the header, and the step-in composer with Stop at the bottom.
- **Covering model picker** (`Shared/ModelPickerSheet.swift`): a full-height sheet
  on iPhone, a large sheet on Mac, searchable, Default first, then the chat role's
  models, then other models, grouped by provider with key status.
- Family colours (`Shared/ModelFamilyStyle.swift`) match the web `--fam-*` tokens
  for light, dark and Increase Contrast.
- **Approval dock** (`Shared/ApprovalDock.swift`, spec 5.13) replaces the chat
  composer while the chat has pending requests, and a thread's step-in composer
  for that thread's own request. There is no approval sheet or banner any more.
  The pending list is `GET /v1/approvals` plus approval events from an
  account-wide event stream (`AppModel.approvalBook`, `GrouperAPI.ApprovalBook`).
- **Approval notifications** (`Shared/ApprovalNotifications.swift`, spec 5.14) on
  iPhone and Mac: local only, while the app runs, after the Settings >
  Notifications opt-in. They have no actions, and tapping one opens the chat's dock.
  APNs is a TODO that needs a paid Apple Developer account.
- **Mac command palette** (`macOS/CommandPaletteView.swift`, spec 5.15): Edit >
  Search… (⌘K) or the sidebar's search button. It has local results only.
  Matching and ranking live in `GrouperAPI.CommandPalette`, which has unit tests.

## Build on a Mac

Requirements: macOS 26 or later, Xcode 26 or later (Xcode 27 for the iOS 27 and macOS 27 SDKs), Swift 6, and XcodeGen.

```sh
brew install xcodegen
cd /path/to/Grouper
scripts/build-apple.sh
cd apple/GrouperApp
xcodegen generate
open Grouper.xcodeproj
```

Select `Grouper-iOS` for an iOS 26 simulator/device or `Grouper-macOS` for the
Mac app. `scripts/build-apple.sh` must regenerate the UniFFI Swift bindings and
native library on the Mac before the Mac target can link. Do not use the checked-in
generated bindings as evidence of a current build. The iPhone target deliberately
depends only on `GrouperAPI`: the design does not run the Rust device node on iOS,
and an iOS XCFramework slice is a later milestone.

The cross-platform API package can also be checked independently:

```sh
cd apple/GrouperKit
swift build --target GrouperAPI
swift test --filter GrouperAPITests
```

## Install a current build

The apps target macOS 27 and iOS 27 first; the deployment target stays 26.

**Mac (Apple Silicon), no Xcode needed:** every CI run on `harness` uploads
`Grouper-macOS-arm64` (kept 14 days). On the run's page in GitHub Actions, download it,
unzip it, then either run `xattr -dr com.apple.quarantine Grouper.app` or open it once
and allow it in System Settings > Privacy & Security > Open Anyway. It's ad-hoc signed
and not notarized, because there's no Apple developer team yet. With `gh`:
`gh run download --repo BlacklionXYZ2/Grouper --name Grouper-macOS-arm64`.

**iPhone:** an iPhone only installs signed apps, so CI's unsigned simulator build can't
go on a phone. From the Mac, with Xcode and a free Apple Account:

1. Build as in "Build on a Mac" below and open `Grouper.xcodeproj`.
2. Xcode > Settings > Accounts: add your Apple Account (a Personal Team appears).
3. For the `Grouper-iOS` and `GrouperWidgets` targets, Signing & Capabilities: tick
   "Automatically manage signing" and pick the Personal Team.
4. Plug in the iPhone, turn on Settings > Privacy & Security > Developer Mode (it
   restarts), select the phone as the run destination and press Run.
5. On the phone, trust the developer in Settings > General > VPN & Device Management.

Free-team installs stop opening after 7 days (press Run again to renew). TestFlight, no
expiry and background push (APNs) need the paid Apple Developer Program.

## CI

`.github/workflows/apple.yml` runs on GitHub's `xcode-27` preview runner (macOS 27) with the
newest stable Xcode there (27.x). It builds the host Mac library with `scripts/build-apple.sh`, runs `swift build`
and `swift test` in `apple/GrouperKit` (live hub tests skip), then builds
`Grouper-macOS` and the iOS Simulator `Grouper-iOS` without code signing, and uploads
the ad-hoc signed Mac app. xcodebuild logs are kept when that build fails. CI does not build an iOS
XCFramework: the script has no iOS slice, and the iPhone target links
`GrouperAPI` only.

## Real-hub API test on Windows

`GrouperAPILiveTests` is skipped unless `GROUPER_LIVE_HUB` and
`GROUPER_LIVE_TOKEN` name a running hub and its admin token. Set
`GROUPER_LIVE_DEVICE_TOKEN` as well to exercise the device-token 403 case. The
test uses real HTTP and WebSocket traffic, starts a project thread, resolves its
manual shell approval, forces a socket reconnect, checks sequence catch-up, and
folds the resulting transcript.

On the Swift 6.4.3 Windows installation used by this repository, run the whole
throwaway fixture with:

```powershell
apple\GrouperKit\Scripts\live-test.ps1
```

The script builds the Rust workspace and confines hub state, provider fixtures,
device workspaces, and logs to the designated `scratchpad\swift-live` directory.
It registers fresh admin/device tokens and stops its hub and device processes in
a `finally` block. Its discovered Swift paths are:

- toolchain: `C:\Users\oscar\AppData\Local\Programs\Swift\Toolchains\6.4.3+Asserts\usr\bin`
- runtime: `C:\Users\oscar\AppData\Local\Programs\Swift\Runtimes\6.4.3\usr\bin`
- SDK: `C:\Users\oscar\AppData\Local\Programs\Swift\Platforms\6.4.3\Windows.platform\Developer\SDKs\Windows.sdk`

The runtime, XCTest, Testing, and toolchain binary directories must be on
`PATH`, and `SDKROOT` must point at that SDK. The script sets all of them for its
child `swift test` process without changing the user's persistent environment.
