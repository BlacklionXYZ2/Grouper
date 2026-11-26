# Grouper apps

The iPhone and Mac apps share the Foundation-only `GrouperAPI` package and the
SwiftUI sources in `Shared/`. The Mac target also links `GrouperKit` for its local
device node. Tokens are stored in the Keychain as `grouper.admin` and
`grouper.device`; they are never written to defaults or logs.

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

`.github/workflows/apple.yml` runs on a `macos-26` runner with the newest stable Xcode
there. It builds the host Mac library with `scripts/build-apple.sh`, runs `swift build`
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
