# AOSP submodules

`hardware/interfaces` is pinned to AOSP commit
`aa229558cbf04bc997ed27465c5e7ad497123d1b`, selected by the
`android-latest-release` manifest for `android16-qpr2-release`.

VPNHotspot uses its frozen AIDL snapshots as source for a generated client-side
Binder proxy:

* `wifi/supplicant/aidl/aidl_api/android.hardware.wifi.supplicant/4`
* `wifi/common/aidl/aidl_api/android.hardware.wifi.common/2`

Refresh this submodule to the latest frozen VINTF version only when the runtime
path intentionally starts using newer transactions.
