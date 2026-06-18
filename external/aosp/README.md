# AOSP submodules

`hardware/interfaces` is pinned to AOSP commit
`0162af698935100a590b7359581ac8b1b80693e5`, selected by the
`android-latest-release` manifest for `android17-release`.

VPNHotspot uses its frozen AIDL snapshots as source for a generated client-side
Binder proxy:

* `wifi/supplicant/aidl/aidl_api/android.hardware.wifi.supplicant/5`
* `wifi/common/aidl/aidl_api/android.hardware.wifi.common/2`

Refresh this submodule to the latest frozen VINTF version only when the runtime
path intentionally starts using newer transactions.
