# Repository Guidelines

## Project Structure & Module Organization
This repo is a single-app Android project. Root Gradle files live at the repo top level. The app module is [`mobile`](./mobile), with Kotlin/Java under [`mobile/src/main/java`](./mobile/src/main/java), native code under [`mobile/src/main/cpp`](./mobile/src/main/cpp), Rust daemon code under [`mobile/src/main/rust`](./mobile/src/main/rust), resources under [`mobile/src/main/res`](./mobile/src/main/res), JVM tests under [`mobile/src/test/java`](./mobile/src/test/java), and instrumented tests under [`mobile/src/androidTest/java`](./mobile/src/androidTest/java). Product-specific source lives in [`mobile/src/google`](./mobile/src/google) and [`mobile/src/freedom`](./mobile/src/freedom).

## Build, Test, and Development Commands
Use Gradle from the repo root.

- `./gradlew :mobile:compileDebugKotlin`: fast Kotlin compile check.
- `./gradlew :mobile:testDebugUnitTest`: run JVM/unit tests.
- `./gradlew :mobile:installDebug`: install the debug build on a connected device.
- `./gradlew :mobile:connectedDebugAndroidTest`: run instrumented tests on device/emulator.

## Coding Style & Naming Conventions
Follow existing Kotlin style: 4-space indentation, concise expression bodies only when clear, and existing naming patterns. Match nearby code before introducing new structure.

- Do not add single-use helpers, wrappers, compat objects, or throwaway one-off classes/data classes unless there is real reuse or a correctness reason.
- Do not break logic into multiple one-off private functions just to "organize" code. If a private function is only called once and does not create real reuse, keep that logic inline in the real entry point.
- Be especially strict about this everywhere. Do not hide one linear control flow behind a pile of `start*`/`stop*`/`update*`/`refresh*` helpers unless each helper has clear reuse or isolates genuinely tricky logic.
- Do not introduce single-use temporary variables that only rename a value for the next line or two. Inline them unless they prevent duplicated work or make a genuinely complex expression clearer.
- Do not keep parallel shadow state in multiple vars when an existing data class or state object can be the single source of truth.
- Do not add defensive `toList()`/`toSet()`/similar copies without a concrete ownership or mutation reason. Keep the copy only when it changes representation, breaks a live view, or protects iteration from mutation.
- Do not use `runCatching` for new code; follow the repo’s normal explicit `try`/`catch` style.
- Do not suppress unexpected exceptions. Best-effort cleanup should catch only the expected failure mode and rethrow the rest.
- Preserve existing comments; do not casually shorten or rewrite them.

## Kotlin Concurrency Design
Prefer resource-owner concurrency over broad locks or global serialization.

- For UI-backed state and lightweight suspending operations, prefer a Main-confined owner using `Dispatchers.Main.immediate`, with explicit in-flight and pending state when operations must run to completion.
- For ordered command or state transitions, prefer a single owner worker, channel, or pending-state loop over launching independent jobs that can interleave.
- Use `Dispatchers.Default.limitedParallelism(1, "...")` for non-UI owner-local mutable state confinement when multiple coroutine entry points need a shared lane, but do not rely on it for run-to-completion ordering across suspensions.
- Use `Mutex` for narrow, local critical sections where the protected invariant is clear. Do not use a daemon/global mutex to hide caller-owned lifecycle races.
- Do not run blocking work on Main. Main-confined owners may call suspending/nonblocking APIs, but blocking I/O, sleeps, or CPU-heavy work must stay off Main.

## Rust Daemon Code Hygiene
Rust daemon code should be event-driven and async-first. Prefer Tokio readiness, cancellation tokens, notifications, channels, and deadline timers over polling loops, fixed sleeps, or manually managed worker threads.

- Avoid large modules. Prefer adding new modules instead of growing existing ones.
- Target Rust modules under 500 LoC, excluding tests.
- If a file exceeds roughly 800 LoC, add new functionality in a new module instead of extending the existing file unless there is a strong documented reason not to.
- When extracting code from a large module, move the related tests and module/type docs toward the new implementation so the invariants stay close to the code that owns them.
- Do not use `std::thread`, blocking worker loops, `spawn_blocking`, or blocking `std::sync::mpsc` patterns unless a blocking platform API leaves no practical alternative. If one is unavoidable, keep it isolated and document why async readiness cannot be used.
- Do not add retry loops with fixed sleeps for steady-state work. Use socket readiness, `AsyncFd`, `Notify`, channels, or `sleep_until` deadlines tied to real protocol timers. Short startup-only retries are acceptable only when no readiness source exists yet.
- Set file descriptors and sockets non-blocking before handing them to Tokio or `AsyncFd`. Do not call blocking `accept`, `recv`, `read`, `write`, DNS, or socket APIs from async tasks.
- Do not silently discard fallible Rust daemon operations. Do not use `let _ = ...`, `.ok()`, `.unwrap_or(...)`, stderr-only logging, or equivalent to hide errors from network I/O, resolver calls, socket sends, netlink, routing, firewall, fd, process, or cleanup operations. If an operation is intentionally best-effort, handle only the expected benign failure mode explicitly.
- Unexpected Rust daemon background failures must be raised to the JVM as structured non-fatal daemon reports. Expected errors may be logged or consumed when the caller intentionally preserves daemon operation.
- Explicit Rust daemon errors returned over IPC must carry structured daemon report context, including useful command/task details, errno when available, and Rust source location. Bare errno/message responses are not sufficient.
- Keep raw `libc` and unsafe code at the owner module boundary that needs it. Prefer `socket2`/Tokio/std APIs when they expose the required behavior, but do not create broad `sys`/`utils` modules just to hide one call site.
- Avoid arbitrary concurrency caps, queue sizes, or timeouts. If a limit is required for resource protection, name it by the resource being limited and justify the chosen value from behavior or platform constraints.
- Local Rust tests are permitted only when they do not introduce non-Android scaffolding into daemon code. If a test needs fake non-Android platform behavior, remove it or refactor the logic under test so the test stays platform-neutral without production fallbacks.
- Run `cargo fmt`, `cargo check`, and preferably `cargo clippy --all-targets -- -D warnings` for Rust changes. Also run the Gradle native build task when the Android build integration could be affected.

## Testing Guidelines
Add or update unit tests in `mobile/src/test/java` for parser, routing, and compatibility logic. Use AndroidX instrumentation tests only when behavior depends on framework/runtime integration. Name tests after the target type, for example `IpSecForwardPolicyCommandTest`. Prefer the smallest test that proves the behavior change.

## Commit & Pull Request Guidelines
Keep commit messages short, imperative, and specific, matching recent history such as `Fixes` or `Update dependencies`. PRs should explain user-visible impact, compatibility risk, and validation performed. Link the issue when relevant and include screenshots only for UI changes.

## Reversible Routing & Root State
Routing, firewall, address, route, and daemon changes should be reversible without relying on app-owned persistent state. Prefer deterministic identifiers and self-describing system/kernel state that `Clean` or reapply can reconstruct after process death, force-stop, reboot, or app data clear.

- Do not make cleanup depend on private app databases, preferences, caches, or in-memory bookkeeping when the state can outlive the app process.
- Prefer idempotent mutations such as replace/delete-by-identifier over add-only operations that require remembering exactly what happened earlier.
- Scope cleanup to mutations this app can identify deterministically. Do not delete or withdraw platform/user state just because it shares an interface or address family.
- Rare platform/setup edge cases do not require extra compatibility machinery when supporting them would add disproportionate routing or lifecycle complexity. It is acceptable for setup to fail in such cases if the failure is explicit, non-silent, and fully reversible by normal cleanup or Clean.
- Root-side changes must document their cleanup path, including what happens during normal stop, Clean, reapply, and interrupted startup.

## Platform API Reflection, Hidden API & Root Changes
Do not hand-wave platform API reflection, hidden API, or root behavior.

- Every reflected Android platform API must be documented, including `sdk/system-api/test-api`.
- Ground documentation in actual code usage in this repo. Check call sites, API guards, and whether the old path is still used.
- README API qualifiers reflect when this app uses the API, not when Android introduced it. If usage spans all supported API levels, omit the qualifier.
- Do not add normal `sdk/public-api` entries to `README.md` just because code now touches them. Document only reflected hidden APIs, blocked APIs, or non-obvious platform assumptions.
- If a hidden API is only used on a narrower runtime path than its Android introduction, the `README.md` qualifier must follow actual app usage, not just platform availability.
- Follow existing conventions first. Check similar entries in `README.md`, `../hiddenapi/hiddenapi-flags.csv`, and nearby source comments before adding new ones.
- Do not search for hiddenapi data elsewhere. Use the provided `../hiddenapi/hiddenapi-flags.csv`, and do not edit it unless explicitly asked.
- Before adding, removing, or reclassifying any Android API entry in `README.md`, verify the exact descriptor and exact overload in `../hiddenapi/hiddenapi-flags.csv`. Do not infer access category from class-level knowledge, AOSP source, or a sibling overload.
- Never guess or synthesize hiddenapi flag suffixes. The suffix after the descriptor, such as `blocked`, `unsupported`, or `sdk,system-api,test-api`, must come from an exact descriptor match in `../hiddenapi/hiddenapi-flags.csv`.
- AOSP API signature files such as `current.txt`, `system-current.txt`, annotations such as `@SystemApi`/`@FlaggedApi`, and SDK stubs may support API-surface or availability conclusions, but they do not prove hiddenapi flags. If the exact descriptor is absent from `../hiddenapi/hiddenapi-flags.csv`, do not append a flag suffix; document the absence explicitly when it matters.
- Treat `public-api` as a stop sign for `Hidden whitelisted APIs` and `Private APIs used / Assumptions for Android customizations` unless this app also uses a different non-public member with its own descriptor.
- Update the correct `README.md` bucket: blocked/private/internal APIs go in `Private APIs used / Assumptions for Android customizations`, reflected `sdk/system-api/test-api` goes in `Hidden whitelisted APIs`, and non-descriptor platform assumptions or AOSP behavior notes go under `Other`.
- Treat `README.md` as a compatibility-hazard index for assumptions that matter if violated.
  - Only put platform behavior under `Other` when this app would misbehave, leak state, or lose required functionality if that behavior differs. Do not document optional fast paths, runtime capability probes, implementation details, public SDK/NDK contracts, essential Linux/kernel facilities, or standard protocol constants when an existing fallback or normal platform contract preserves correctness.
  - Hardcoded AOSP-derived constants/values must be documented inline. Represent them in `README.md` only when they map to a hidden/private platform symbol or to a non-obvious compatibility assumption that would break app behavior if changed.
- Document the access point inline in the existing repo style.
- Source-backed platform notes attached to a field, class, or method must use a declaration doc comment `/** ... */`, not plain `//`.
- Always verify behavior and introduction points against actual AOSP source, using both the earliest verified `android-*_r1` with exact line numbers and the current local `main`-style checkout when the task depends on latest behavior too.
- Before introducing a blocked API, explicitly identify the less-restricted alternative considered and why it is insufficient for this caller.
- Document cleanup/revert behavior for root-side changes, especially for `Clean`/reapply and process-death leakage.
- Do not reflect from `object.javaClass` or other runtime instance classes unless there is a specific reason. Cache the owning platform class/member with `lazy`.
- When a platform API is public but runtime availability may vary due to Mainline/APEX, prefer direct typed use behind runtime capability detection. Use reflection only for presence/signature detection when `SDK_INT` is insufficient; do not reflect public accessors purely to avoid imports.
- Follow existing reflected-name conventions: use `clazz` only when the surrounding type already names the unambiguous platform class; otherwise use `classFoo`. Use `getFoo`/`setFoo`/field names for reflected members.
- Never guess about usage, API levels, descriptors, relocation, or platform behavior. Resolve uncertainty from repo code, `../hiddenapi`, and AOSP before updating code or docs.

Keep `README.md` in sync with these changes.

- Update `Private APIs used / Assumptions for Android customizations`, `Hidden whitelisted APIs`, and `Other` whenever descriptors, API ranges, hidden constants, or platform assumptions change.
- Cross-check README entries against `../hiddenapi/hiddenapi-flags.csv` by exact descriptor when applicable.
- If `README.md` API documentation changes, state in the final response that `../hiddenapi/hiddenapi-flags.csv` was checked and which descriptors were verified.
- If a change affects compatibility, cleanup behavior, or required privileges, also update the relevant README usage or troubleshooting text.
