# Grouper apps

The iPhone and Mac apps share the Foundation-only `GrouperAPI` package and the
SwiftUI sources in `Shared/`. The Mac target also links `GrouperKit` for its local
device node. Tokens are stored in the Keychain as `grouper.admin` and
`grouper.device`; they are never written to defaults or logs.

## Build on a Mac

Requirements: macOS 26, Xcode 26, Swift 6, and XcodeGen.

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
