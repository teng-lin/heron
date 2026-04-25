# Swift bridge pattern

Every Swift helper that ships in v1 (`whisperkit-bridge`, `zoom-ax-backend`,
`keychain-helper`) follows the same shape, established by
`swift/eventkit-helper/` and documented here. The canonical source of
truth is [`docs/implementation.md`](implementation.md) §5.4 — this
document is the condensed checklist.

## Layout

```
swift/<helper-name>/
├── Package.swift
└── Sources/<HelperName>/
    └── <HelperName>.swift
```

## `Package.swift`

```swift
// swift-tools-version:5.9
import PackageDescription
let package = Package(
    name: "<HelperName>",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "<HelperName>", type: .static, targets: ["<HelperName>"]),
    ],
    targets: [.target(name: "<HelperName>")]
)
```

`type: .static` is load-bearing — Rust links the `.a` directly.
A dynamic library would force runtime dylib resolution we don't need.

## Swift entry points

Every entry point is `@_cdecl` so it presents a stable C symbol name.
Three rules:

1. **Strings are owned by the side that allocated them.** Swift returns
   strings via `strndup` (so they live in the C heap); Rust calls a
   paired `<helper>_free_string` to give ownership back to Swift.
2. **Async APIs are wrapped in a synchronous façade.** Swift concurrency
   (`Task`, `await`) doesn't cross FFI cleanly. Spawn a `Task.detached`,
   block on a `DispatchSemaphore`. The detached task runs on its own
   queue, so `sem.wait()` on the caller's thread cannot deadlock.
3. **Errors collapse to integer return codes** at the boundary. The Rust
   side decides whether the failure is recoverable; Swift just signals
   "happened" or "didn't."

Example:

```swift
@_cdecl("ek_request_access")
public func ek_request_access() -> Int32 {
    var result: Int32 = 0
    let sem = DispatchSemaphore(value: 0)
    Task.detached {
        do { result = try await store.requestFullAccessToEvents() ? 1 : 0 }
        catch { result = 0 }
        sem.signal()
    }
    sem.wait()
    return result
}
```

## Rust `build.rs`

Each consumer crate has a `build.rs` that:

1. Runs `swift build -c release --arch <host_arch>` inside the helper
   directory.
2. Adds the resulting `.build/<triple>/release` to
   `cargo:rustc-link-search=native`.
3. Emits `cargo:rustc-link-lib=static=<HelperName>` plus
   `cargo:rustc-link-lib=framework=<each Apple framework>` for whatever
   the helper imports (EventKit, Foundation, AVFoundation, …).
4. Adds the toolchain's `swift-5.5/macosx` as a link-time search path
   (so unresolved symbols resolve) and `/usr/lib/swift` as an `rpath`
   (so dyld finds the Swift runtime via the system shared cache at
   load time). Adding the toolchain dir as the rpath also works but
   produces "Class … implemented in both …" warnings due to duplicate
   loads via EventKit/AVFoundation transitive deps.
5. Calls `cargo:rerun-if-changed` on the helper directory and on the
   build script itself.

The canonical implementation lives at `crates/heron-vault/build.rs`.
Copy that file as the starting point for new bridges.

## Rust FFI shim

A small module in the consumer crate that:

- declares the `extern "C"` block (one symbol per `@_cdecl` export);
- gates all calendar/audio/etc. code with `#[cfg(target_vendor = "apple")]`;
- provides safe Rust wrappers that own the unsafe.

See `crates/heron-vault/src/calendar.rs` for the canonical shape.

## Testing

The boundary verification command is

```sh
cargo test -p <consumer-crate> <bridge>_smoke -- --ignored
```

The smoke test is **always `#[ignore]`d** in CI because it triggers a
TCC prompt the first time, and CI has no human to click it. Run
manually after `scripts/reset-onboarding.sh` to verify the bridge from
a clean TCC state.

## Exceptions: single-file binaries

`swift/ax-probe/main.swift` is a single-file binary built directly
with `swiftc`, not a Package. This carve-out is for **executable spike
tools only**, never for v1 production bridges that link into Rust.
