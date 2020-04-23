# VPN Hotspot

[![CircleCI](https://circleci.com/gh/Mygod/VPNHotspot.svg?style=shield)](https://circleci.com/gh/Mygod/VPNHotspot)
[![API](https://img.shields.io/badge/API-21%2B-brightgreen.svg?style=flat)](https://android-arsenal.com/api?level=21)
[![Releases](https://img.shields.io/github/downloads/Mygod/VPNHotspot/total.svg)](https://github.com/Mygod/VPNHotspot/releases)
[![Language: Kotlin](https://img.shields.io/github/languages/top/Mygod/VPNHotspot.svg)](https://github.com/Mygod/VPNHotspot/search?l=kotlin)
[![Codacy Badge](https://api.codacy.com/project/badge/Grade/e70e52b1a58045819b505c09edcae816)](https://www.codacy.com/app/Mygod/VPNHotspot?utm_source=github.com&amp;utm_medium=referral&amp;utm_content=Mygod/VPNHotspot&amp;utm_campaign=Badge_Grade)
[![License](https://img.shields.io/github/license/Mygod/VPNHotspot.svg)](LICENSE)

Connecting things to your VPN made simple. Share your VPN connection over hotspot or repeater. (**root required**)  
<a href="https://play.google.com/store/apps/details?id=be.mygod.vpnhotspot" target="_blank"><img src="https://play.google.com/intl/en_us/badges/images/generic/en-play-badge.png" height="60"></a>,
<a href="https://labs.xda-developers.com/store/app/be.mygod.vpnhotspot" target="_blank">XDA Labs</a>,
<a href="https://appdistribution.firebase.dev/i/FUCPGdzm" target="_blank">sign up for beta</a>

This app is useful for:

* Connecting things that don't support VPN like Chromecasts behind corporate firewalls;
* Setting up [gapps](https://support.google.com/pixelphone/answer/7158475) behind corporate firewalls;
* Connecting to your mobile hotspot but you're not bothered to set up VPN on your device;
* Identifying, monitoring and blocking (unwanted) clients;
* Bypassing tethering limits by:
    - (recommended) Use this app with a real VPN/socksifier;
    - Use this app with some adblock/DNS apps that uses system VPN service APIs, i.e. fake VPNs; ([troubleshooting/a list of apps that work](https://github.com/Mygod/VPNHotspot/issues/139))
    - Try your luck and simply use this app.

P.S. You can also do the similar on [Windows](https://www.expressvpn.com/support/vpn-setup/share-vpn-connection-windows/),
[Mac](https://www.expressvpn.com/support/vpn-setup/share-vpn-connection-mac/),
and [iOS](http://www.tetherme.net/).
I don't know about you but I can't get my stupid Windows 10 to work with
[hosted network](https://msdn.microsoft.com/en-us/library/windows/desktop/dd815243(v=vs.85).aspx)
now that they introduced this
[Mobile hotspot](https://support.microsoft.com/en-us/help/4027762/windows-use-your-pc-as-a-mobile-hotspot).

## Settings and How to Use Them

Default settings are picked to suit general use cases and maximize compatibility but it might not be optimal for battery
 life.

### Upstream

* Upstream network interface: Main upstream used to reroute traffic.
  Leave blank for auto detect system VPN (allow/do not bypass this app to use VPN for it to work).
  Put `none` (or other similarly invalid entries) to suppress tethering VPN.
* Fallback upstream:
  Fallback upstream is used when some VPN leave certain routes fallback to default network interface.
  Leave blank for auto detect.
  Put `none` (or other similarly invalid entries) to forbid falling back.
  Put other interface name if you feel like it.
* IP Masquerade Mode:
  - None:
    Nothing will be done to remap address/port from downstream.
    I find turning this option off sometimes works better for dummy VPNs like ad-blockers and socksifiers than Simple mode, e.g. Shadowsocks.
    But you should never use this for real VPNs like OpenVPN, etc.
  - Simple: Source address/port from downstream packets will be remapped and that's about it.
  - (since Android 9) Android Netd Service:
    Let your system handle masquerade.
    Android system will do a few extra things to make things like FTP and tethering traffic counter work.
    You should probably not use this if you are trying to hide your tethering activity from your carrier.

### Downstream

* Disable IPv6 tethering: Turning this option on will disable IPv6 for system tethering. Useful for stopping IPv6 leaks
  as this app currently doesn't handle IPv6 VPN tethering (see [#6](https://github.com/Mygod/VPNHotspot/issues/6)).
* (since Android 8.1) Tethering hardware acceleration:
    This is a shortcut to the same setting in system Developer options.
    Turning this option off is probably a must for making VPN tethering over system tethering work,
     but it might also decrease your battery life while tethering is enabled.
* Enable DHCP workaround:
    Only used if your device isn't able to get your clients IP addresses with VPN on.
    This is a global setting, meaning it will only be applied once globally.

### Misc

* Keep Wi-Fi alive: Acquire Wi-Fi locks when repeater, temporary hotspot or system VPN hotspot is activated.
   - Choose "System default" (default since Android 10) to save battery life;
   - (prior to Android 10) Choose "On" (default) if repeater/hotspot turns itself off automatically or stops working after a while;
   - (prior to Android 10) Choose "High Performance Mode" to minimize packet loss and latency (will consume more power);
   - (since Android 10) Choose "Disable power save" to decrease packet latency.
     An example use case is when a voice connection needs to be kept active even after the device screen goes off.
     Using this mode may improve the call quality.
     Requires support from the hardware.
   - (since Android 10) Choose "Low latency mode" to optimize for reduced packet latency, and this might result in:
     1. Reduced battery life.
     2. Reduced throughput.
     3. Reduced frequency of Wi-Fi scanning.
        This may cause the device not roaming or switching to the AP with highest signal quality, and location accuracy may be reduced.
     Example use cases are real time gaming or virtual reality applications where low latency is a key factor for user experience.
     Requires support from the hardware.
     Note: Requires this app running in foreground with screen on.
* Start repeater on boot: Self explanatory.
* Repeater safe mode: (Android 10, March 2020 security patch or newer)
  You might be required to turn this mode off if you want to use short SSID (at most 8 bytes long).
  Unsafe mode might not work for your device, and there is a small chance you will soft brick your device (recoverable).
  See [#153](https://github.com/Mygod/VPNHotspot/issues/153) for more information.
* Network status monitor mode: This option controls how the app monitors connected devices as well as interface changes
  (when custom upstream is used).
  Requires restarting the app to take effects. (best way is to go to app info and force stop)
   - Netlink monitor: Use Linux netlink mechanism, most battery efficient but may not work with SELinux enforcing mode.
     Sometimes auto fallbacks to Netlink monitor with root and Poll.
   - Netlink monitor with root: Same as above but runs netlink as root. This option works well with SELinux enforcing mode
     but might still be bugged on devices heavily modified by OEM and/or carriers. Sometimes auto fallbacks to Poll.
   - Poll: (default) Update network information manually every second. Least battery efficient but it should work on most
     devices. Recommended to switch to other modes if possible.
   - Poll with root: Same as Poll but polling is done using a root shell.


## Q & A

Search the [issue tracker](https://github.com/Mygod/VPNHotspot/issues) for more.

### [What changes exactly can this app do to my system? (and how to revert them)](https://github.com/Mygod/VPNHotspot/issues/8#issuecomment-448529512)

### [No root?](https://github.com/Mygod/VPNHotspot/issues/62)

### Failed to create group due to internal error/repeater shuts down after a while?

This could caused by the Wi-Fi channel you selected is no longer available, due to:

1. Your device doesn't support operating on this channel, or
2. There is some nearby Wi-Fi direct device that broadcasted that it can't operate on the channel you picked.

For maximum stability, you need to set channel = 0 so that your device will pick a channel automatically.
You can also use WPS to connect your 2.4GHz-only device to force the repeater to switch from 5GHz to 2.4GHz for this time.

## Private APIs used / Assumptions for Android customizations

_a.k.a. things that can go wrong if this app doesn't work._

This is a list of stuff that might impact this app's functionality if unavailable.
This is only meant to be an index. You can read more in the source code.

Non-public API list:

* (since API 24) [`Landroid/bluetooth/BluetoothPan;->isTetheringOn()Z,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#32103)
* (since API 24) [`Landroid/net/ConnectivityManager$OnStartTetheringCallback;-><init>()V,system-api,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#123103)
* (since API 24) [`Landroid/net/ConnectivityManager$OnStartTetheringCallback;->onTetheringFailed()V,system-api,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#123104)
* (since API 24) [`Landroid/net/ConnectivityManager$OnStartTetheringCallback;->onTetheringStarted()V,system-api,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#123105)
* (since API 24) [`Landroid/net/ConnectivityManager;->getLastTetherError(Ljava/lang/String;)I,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#123309)
* (since API 24) [`Landroid/net/ConnectivityManager;->startTethering(IZLandroid/net/ConnectivityManager$OnStartTetheringCallback;Landroid/os/Handler;)V,system-api,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#123408)
* (since API 24) [`Landroid/net/ConnectivityManager;->stopTethering(I)V,system-api,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#123410)
* (since API 23) [`Landroid/net/wifi/WifiConfiguration;->apBand:I,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#131529)
* (since API 23) [`Landroid/net/wifi/WifiConfiguration;->apChannel:I,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#131530)
* (since API 26) [`Landroid/net/wifi/WifiManager;->cancelLocalOnlyHotspotRequest()V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#132250)
* [`Landroid/net/wifi/WifiManager;->getWifiApConfiguration()Landroid/net/wifi/WifiConfiguration;,system-api,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#132289)
* [`Landroid/net/wifi/WifiManager;->setWifiApConfiguration(Landroid/net/wifi/WifiConfiguration;)Z,system-api,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#132358)
* (deprecated since API 26) `Landroid/net/wifi/WifiManager;->setWifiApEnabled(Landroid/net/wifi/WifiConfiguration;Z)Z`
* [`Landroid/net/wifi/p2p/WifiP2pGroup;->getNetworkId()I,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#134440)
* [`Landroid/net/wifi/p2p/WifiP2pGroupList;->getGroupList()Ljava/util/Collection;,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#134487)
* [`Landroid/net/wifi/p2p/WifiP2pManager;->deletePersistentGroup(Landroid/net/wifi/p2p/WifiP2pManager$Channel;ILandroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#134703)
* [`Landroid/net/wifi/p2p/WifiP2pManager;->requestPersistentGroupInfo(Landroid/net/wifi/p2p/WifiP2pManager$Channel;Landroid/net/wifi/p2p/WifiP2pManager$PersistentGroupInfoListener;)V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#134728)
* [`Landroid/net/wifi/p2p/WifiP2pManager;->setWifiP2pChannels(Landroid/net/wifi/p2p/WifiP2pManager$Channel;IILandroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#134737)
* [`Landroid/net/wifi/p2p/WifiP2pManager;->startWps(Landroid/net/wifi/p2p/WifiP2pManager$Channel;Landroid/net/wifi/WpsInfo;Landroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#134738)
* (prior to API 29) [`Ljava/net/InetAddress;->parseNumericAddress(Ljava/lang/String;)Ljava/net/InetAddress;,core-platform-api,greylist-max-p`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#335306)

Undocumented system configurations:

* `@android:array/config_tether_usb_regexs`
* `@android:array/config_tether_wifi_regexs`
* `@android:array/config_tether_wimax_regexs`
* `@android:array/config_tether_bluetooth_regexs`
* (since API 28) `@android:integer/config_wifi_framework_soft_ap_timeout_delay`

Other:

* (since API 29) `android.net.wifi.p2p.WifiP2pConfig` needs to be parcelized in a very specific order, except for possible extra fields at the end. (used only for safe mode)
* (since API 28) [`Landroid/provider/Settings$Global;->SOFT_AP_TIMEOUT_ENABLED:Ljava/lang/String;,greylist-max-o`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#158307) is assumed to be `"soft_ap_timeout_enabled"`.
* (since API 27) [`Landroid/provider/Settings$Global;->TETHER_OFFLOAD_DISABLED:Ljava/lang/String;,greylist-max-o`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#158331) is assumed to be `"tether_offload_disabled"`.
* (since API 27) `com.android.server.connectivity.tethering.OffloadHardwareInterface.DEFAULT_TETHER_OFFLOAD_DISABLED` is assumed to be 0.
* Several constants in `ConnectivityManager` is assumed to be defined as in `TetheringManager.kt`;
* Following broadcasts are assumed to be sticky:
  - [`Landroid/net/ConnectivityManager;->ACTION_TETHER_STATE_CHANGED:Ljava/lang/String;,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#123163) is assumed to be `android.net.conn.TETHER_STATE_CHANGED`.
  - [`Landroid/net/wifi/p2p/WifiP2pManager;->WIFI_P2P_PERSISTENT_GROUPS_CHANGED_ACTION:Ljava/lang/String;,greylist-max-o`](https://android.googlesource.com/platform/prebuilts/runtime/+/3d07e5c/appcompat/hiddenapi-flags.csv#134686) is assumed to be `android.net.wifi.p2p.PERSISTENT_GROUPS_CHANGED`;
* Activity `com.android.settings/.Settings$TetherSettingsActivity` is assumed to be exported.

For `ip rule` priorities, `RULE_PRIORITY_SECURE_VPN` and `RULE_PRIORITY_TETHERING` is assumed to be 12000 and 18000 respectively;
(prior to API 24) `RULE_PRIORITY_DEFAULT_NETWORK` is assumed to be 22000 (or at least > 18000).
DHCP server like `dnsmasq` is assumed to run and send DHCP packets as root.

Undocumented system binaries are all bundled and executable:

* (since API 24) `iptables-save`, `ip6tables-save`;
* `echo`;
* `ip` (`link monitor neigh rule`);
* `ndc` (`ipfwd` since API 23, `nat` since API 28);
* `iptables`, `ip6tables` (with correct version corresponding to API level, `-nvx -L <chain>`);
* `su`.

If some of these are unavailable, you can alternatively install a recent version (v1.28.1 or higher) of Busybox.

Wi-Fi driver `wpa_supplicant`:

* P2P configuration file is assumed to be saved to [`/data/vendor/wifi/wpa/p2p_supplicant.conf` or `/data/misc/wifi/p2p_supplicant.conf`](https://android.googlesource.com/platform/external/wpa_supplicant_8/+/0b4856b6dc451e290f1f64f6af17e010be78c073/wpa_supplicant/hidl/1.1/supplicant.cpp#26) and have reasonable format;
* Android system is expected to restart `wpa_supplicant` after it crashes.
