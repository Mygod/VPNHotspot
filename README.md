# VPN Hotspot

[![CircleCI](https://circleci.com/gh/Mygod/VPNHotspot.svg?style=shield)](https://circleci.com/gh/Mygod/VPNHotspot)
[![API](https://img.shields.io/badge/API-21%2B-brightgreen.svg?style=flat)](https://android-arsenal.com/api?level=21)
[![Releases](https://img.shields.io/github/downloads/Mygod/VPNHotspot/total.svg)](https://github.com/Mygod/VPNHotspot/releases)
[![F-Droid](https://img.shields.io/f-droid/v/be.mygod.vpnhotspot.svg)](https://f-droid.org/en/packages/be.mygod.vpnhotspot)
[![Language: Kotlin](https://img.shields.io/github/languages/top/Mygod/VPNHotspot.svg)](https://github.com/Mygod/VPNHotspot/search?l=kotlin)
[![Codacy Badge](https://api.codacy.com/project/badge/Grade/e70e52b1a58045819b505c09edcae816)](https://www.codacy.com/app/Mygod/VPNHotspot?utm_source=github.com&amp;utm_medium=referral&amp;utm_content=Mygod/VPNHotspot&amp;utm_campaign=Badge_Grade)
[![License](https://img.shields.io/github/license/Mygod/VPNHotspot.svg)](LICENSE)
[![FOSSA Status](https://app.fossa.io/api/projects/git%2Bgithub.com%2FMygod%2FVPNHotspot.svg?type=shield)](https://app.fossa.io/projects/git%2Bgithub.com%2FMygod%2FVPNHotspot?ref=badge_shield)

Connecting things to your VPN made simple. Share your VPN connection over hotspot or repeater. (**root required**)  
<a href="https://play.google.com/store/apps/details?id=be.mygod.vpnhotspot" target="_blank"><img src="https://play.google.com/intl/en_us/badges/images/generic/en-play-badge.png" height="60"></a>
or <a href="https://labs.xda-developers.com/store/app/be.mygod.vpnhotspot" target="_blank">XDA Labs</a>

This app is useful for:

* Connecting things that don't support VPN like Chromecasts behind corporate firewalls;
* Setting up [gapps](https://support.google.com/pixelphone/answer/7158475) behind corporate firewalls;
* Connecting to your mobile hotspot but you're not bothered to set up VPN on your device;
* Identifying, monitoring and blocking (unwanted) clients;
* Bypassing tethering limits by:
    - (recommended) Use this app with a real VPN/socksifier; (ad-blocker may not work)
    - (might or might not work based on your carrier) Use this app with TTL editor like
      [TTL Master](https://github.com/TTLMaster/TTLMaster);
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

* Upstream network interface: Main upstream used to reroute traffic. Leave blank for auto detect system VPN.
  Put `none` (or other similarly invalid entries) to suppress tethering VPN.
  Put your interface name for tethering WireGuard.
* IP Masquerade Mode:
  - None:
    Nothing will be done to remap address/port from downstream.
    I find turning this option off sometimes works better for dummy VPNs like ad-blockers and socksifiers than Simple mode, e.g. Shadowsocks.
    But you should never use this for real VPNs like OpenVPN, WireGuard, etc.
  - Simple: Source address/port from downstream packets will be remapped and that's about it.
  - Android Netd Service:
    Let your system handle masquerade.
    Android system will do a few extra things to make things like FTP and tethering traffic counter work.
    You should probably not use this if you are trying to hide your tethering activity from your carrier.

### Downstream

* Disable IPv6 tethering: Turning this option on will disable IPv6 for system tethering. Useful for stopping IPv6 leaks
  as this app currently doesn't handle IPv6 VPN tethering (see [#6](https://github.com/Mygod/VPNHotspot/issues/6)).
* Enable DHCP workaround:
    Only used if your device isn't able to get your clients IP addresses with VPN on.
    This is a global setting, meaning it will only be applied once globally.

### Misc

* Keep Wi-Fi alive: Acquire Wi-Fi locks when repeater, temporary hotspot or system VPN hotspot is activated.
   - Choose "System default" to save battery life;
   - Choose "On" (default) if repeater/hotspot turns itself off automatically or stops working after a while;
   - Choose "High Performance Mode" to minimize packet loss and latency (will consume more power).
* Start repeater on boot: Self explanatory.
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

### [Unable to find "Tethering hardware acceleration" in Developer options?](https://github.com/Mygod/VPNHotspot/issues/41#issuecomment-425732001)

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

Undocumented API list:

* (since API 24) [`Landroid/bluetooth/BluetoothPan;->isTetheringOn()Z,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#28703)
* (since API 24) [`Landroid/net/ConnectivityManager$OnStartTetheringCallback;-><init>()V,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#112695)
* (since API 24) [`Landroid/net/ConnectivityManager$OnStartTetheringCallback;->onTetheringFailed()V,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#112696)
* (since API 24) [`Landroid/net/ConnectivityManager$OnStartTetheringCallback;->onTetheringStarted()V,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#112697)
* (since API 24) [`Landroid/net/ConnectivityManager;->getLastTetherError(Ljava/lang/String;)I,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#112882)
* (since API 24) [`Landroid/net/ConnectivityManager;->startTethering(IZLandroid/net/ConnectivityManager$OnStartTetheringCallback;Landroid/os/Handler;)V,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#112972)
* (since API 24) [`Landroid/net/ConnectivityManager;->stopTethering(I)V,whitelist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#112974)
* [`Landroid/net/wifi/p2p/WifiP2pGroup;->getNetworkId()I,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#123194)
* [`Landroid/net/wifi/p2p/WifiP2pGroupList;->getGroupList()Ljava/util/Collection;,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#123239)
* [`Landroid/net/wifi/p2p/WifiP2pManager;->deletePersistentGroup(Landroid/net/wifi/p2p/WifiP2pManager$Channel;ILandroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#123431)
* [`Landroid/net/wifi/p2p/WifiP2pManager;->requestPersistentGroupInfo(Landroid/net/wifi/p2p/WifiP2pManager$Channel;Landroid/net/wifi/p2p/WifiP2pManager$PersistentGroupInfoListener;)V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#123450)
* [`Landroid/net/wifi/p2p/WifiP2pManager;->setWifiP2pChannels(Landroid/net/wifi/p2p/WifiP2pManager$Channel;IILandroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#123458)
* [`Landroid/net/wifi/p2p/WifiP2pManager;->startWps(Landroid/net/wifi/p2p/WifiP2pManager$Channel;Landroid/net/wifi/WpsInfo;Landroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#123459)
* [`Ljava/net/InetAddress;->parseNumericAddress(Ljava/lang/String;)Ljava/net/InetAddress;,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#299587)
* (deprecated since API 26) `Landroid/net/wifi/WifiManager;->setWifiApEnabled(Landroid/net/wifi/WifiConfiguration;Z)Z`

Undocumented system configurations:

* `@android:array/config_tether_usb_regexs`
* `@android:array/config_tether_wifi_regexs`
* `@android:array/config_tether_wimax_regexs`
* `@android:array/config_tether_bluetooth_regexs`

Other:

* (since API 27) [`Landroid/provider/Settings$Global;->TETHER_OFFLOAD_DISABLED:Ljava/lang/String;,greylist-max-o`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#144760) is assumed to be `"tether_offload_disabled"`.
* (since API 27) `com.android.server.connectivity.tethering.OffloadHardwareInterface.DEFAULT_TETHER_OFFLOAD_DISABLED` is assumed to be 0.
* Several constants in `ConnectivityManager` is assumed to be defined as in `TetheringManager.kt`;
* Following broadcasts are assumed to be sticky:
  - [`Landroid/net/ConnectivityManager;->ACTION_TETHER_STATE_CHANGED:Ljava/lang/String;,greylist`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#112743) is assumed to be `android.net.conn.TETHER_STATE_CHANGED`.
  - [`Landroid/net/wifi/p2p/WifiP2pManager;->WIFI_P2P_PERSISTENT_GROUPS_CHANGED_ACTION:Ljava/lang/String;,greylist-max-o`](https://android.googlesource.com/platform/prebuilts/runtime/+/aa21a6e/appcompat/hiddenapi-flags.csv#123415) is assumed to be `android.net.wifi.p2p.PERSISTENT_GROUPS_CHANGED`;
* Activity `com.android.settings/.Settings$TetherSettingsActivity` is assumed to be exported.

For `ip rule` priorities, `RULE_PRIORITY_SECURE_VPN` and `RULE_PRIORITY_TETHERING` is assumed to be 12000 and 18000
respectively; `RULE_PRIORITY_DEFAULT_NETWORK` is assumed to be 22000 (or at least > 18000) for API 27-.
DHCP server like `dnsmasq` is assumed to run and send DHCP packets as root.

Undocumented system binaries are all bundled and executable:

* Since API 24: `iptables-save`;
* `echo`;
* `ip` (`link monitor neigh rule` with proper output format);
* `ndc` (`ipfwd` with proper output format since API 23, `nat`);
* `iptables` (with correct version corresponding to API level, `-nvx -L <chain>` with proper output format);
* `su`.

If some of these are unavailable, you can alternatively install a recent version (v1.28.1 or higher) of Busybox.

Wi-Fi driver `wpa_supplicant`:

* P2P configuration file is assumed to be saved to [`/data/vendor/wifi/wpa/p2p_supplicant.conf` or `/data/misc/wifi/p2p_supplicant.conf`](https://android.googlesource.com/platform/external/wpa_supplicant_8/+/0b4856b6dc451e290f1f64f6af17e010be78c073/wpa_supplicant/hidl/1.1/supplicant.cpp#26) and have reasonable format;
* Android system is expected to restart `wpa_supplicant` after it crashes.


## License
[![FOSSA Status](https://app.fossa.io/api/projects/git%2Bgithub.com%2FMygod%2FVPNHotspot.svg?type=large)](https://app.fossa.io/projects/git%2Bgithub.com%2FMygod%2FVPNHotspot?ref=badge_large)