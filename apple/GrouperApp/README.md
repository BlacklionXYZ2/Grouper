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
