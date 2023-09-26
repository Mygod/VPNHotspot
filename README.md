# VPN Hotspot

[![CircleCI](https://circleci.com/gh/Mygod/VPNHotspot.svg?style=shield)](https://circleci.com/gh/Mygod/VPNHotspot)
[![API](https://img.shields.io/badge/API-28%2B-brightgreen.svg?style=flat)](https://android-arsenal.com/api?level=28)
[![Releases](https://img.shields.io/github/downloads/Mygod/VPNHotspot/total.svg)](https://github.com/Mygod/VPNHotspot/releases)
[![Language: Kotlin](https://img.shields.io/github/languages/top/Mygod/VPNHotspot.svg)](https://github.com/Mygod/VPNHotspot/search?l=kotlin)
[![Codacy Badge](https://app.codacy.com/project/badge/Grade/e70e52b1a58045819b505c09edcae816)](https://www.codacy.com/gh/Mygod/VPNHotspot/dashboard?utm_source=github.com&amp;utm_medium=referral&amp;utm_content=Mygod/VPNHotspot&amp;utm_campaign=Badge_Grade)
[![License](https://img.shields.io/github/license/Mygod/VPNHotspot.svg)](LICENSE)

<a href="https://play.google.com/store/apps/details?id=be.mygod.vpnhotspot"><img src="https://play.google.com/intl/en_us/badges/images/generic/en-play-badge.png" height="48"></a>
([beta](https://play.google.com/apps/testing/be.mygod.vpnhotspot))
SHA-256 fingerprint of the [code transparency](https://developer.android.com/guide/app-bundle/code-transparency#verify_code_transparency) key certificate (must be compared with the developer's public key manually): `C8 70 92 46 B3 32 22 6F 33 57 05 B4 5C 37 83 81 65 67 42 44 1B DC CC B2 96 1B 66 4D CF 4C 74 BC`

Connecting things to your VPN made simple. Share your VPN connection over hotspot or repeater. (**root required**)  

This app is useful for:

* Connecting things that don't support VPN like Chromecasts behind corporate firewalls;
* Setting up [gapps](https://support.google.com/pixelphone/answer/7158475) behind corporate firewalls;
* Connecting to your mobile hotspot but you're not bothered to set up VPN on your device;
* Identifying, monitoring and blocking (unwanted) clients;
* Bypassing tethering limits by:
    - (recommended) Use this app with a real VPN/socksifier;
    - Use this app with some adblock/DNS apps that uses system VPN service APIs, i.e. fake VPNs; ([troubleshooting/a list of apps that work](https://github.com/Mygod/VPNHotspot/discussions/216))
    - Try your luck and simply use this app.

P.S. You can also do the similar on [Windows](https://www.expressvpn.com/support/vpn-setup/share-vpn-connection-windows/),
[Mac](https://www.expressvpn.com/support/vpn-setup/share-vpn-connection-mac/),
and [iOS](http://www.tetherme.net/).
I don't know about you but I can't get my stupid Windows 10 to work with
[hosted network](https://msdn.microsoft.com/en-us/library/windows/desktop/dd815243(v=vs.85).aspx)
now that they introduced this
[Mobile hotspot](https://support.microsoft.com/en-us/help/4027762/windows-use-your-pc-as-a-mobile-hotspot).

## Features That Requires System App Installation

The following features in the app requires it to be installed under `/system/priv-app` since some restricted permissions are required.
One way to do this is to use [App systemizer for Magisk](https://github.com/Magisk-Modules-Repo/terminal_systemizer).

* (Android 8-10, since app v2.4.0) `android.permission.OVERRIDE_WIFI_CONFIG`: Read/write system Wi-Fi hotspot configuration. ([#117](https://github.com/Mygod/VPNHotspot/issues/117))

Installing as system app also has the side benefit of launching root daemon less frequently due to having privileged permissions listed below.

* `android.permission.LOCAL_MAC_ADDRESS`
* `android.permission.MANAGE_USB`
* `android.permission.OVERRIDE_WIFI_CONFIG`
* `android.permission.READ_WIFI_CREDENTIAL`
* `android.permission.TETHER_PRIVILEGED`
* `android.permission.WRITE_SECURE_SETTINGS`

Whenever you install an app update, if there was a new protected permission addition (last updated in v2.10.4), you should update the app installed in system as well to make the system grant the privileged permission.

## Settings and How to Use Them

Default settings are picked to suit general use cases and maximize compatibility but it might not be optimal for battery
 life.

### Upstream

* Upstream network interface: Main upstream regex used to reroute traffic.
  Leave blank for auto detect system VPN (allow/do not bypass this app to use VPN for it to work).
  Put `none` (or `a^` or other similarly invalid entries) to suppress tethering VPN.
* Fallback upstream:
  Fallback upstream is used when some VPN leave certain routes fallback to default network interface.
  Leave blank for auto detect.
  Put `none` (or `a^` or other similarly invalid entries) to forbid falling back.
  Put other interface name if you feel like it.
* IP Masquerade Mode:
  - None:
    Nothing will be done to remap address/port from downstream.
    I find turning this option off sometimes works better for dummy VPNs like ad-blockers and socksifiers than Simple mode, e.g. Shadowsocks.
    But you should never use this for real VPNs like OpenVPN, etc.
  - Simple: Source address/port from downstream packets will be remapped and that's about it.
  - Android Netd Service:
    Let your system handle masquerade.
    Android system will do a few extra things to make things like FTP and tethering traffic counter work.
    You should probably not use this if you are trying to hide your tethering activity from your carrier.

### Downstream

* Disable IPv6 tethering: Turning this option on will disable IPv6 for system tethering. Useful for stopping IPv6 leaks
  as this app currently doesn't handle IPv6 VPN tethering (see [#6](https://github.com/Mygod/VPNHotspot/issues/6)).
* Tethering hardware acceleration:
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
     Deprecated in Android 14 and is automatically replaced with "Low latency mode".
     Deprecation is due to the impact of it on power dissipation.
     The "Low latency mode" provides much of the same desired functionality with less impact on power dissipation.
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
* Use system configuration for temporary hotspot: (Android 11 or newer)
  Attempt to start a temporary hotspot using system Wi-Fi hotspot configuration.
  This feature is most likely only functional on Android 12 or newer.
  Enabling this switch will also prevent other apps from using the [local-only hotspot](https://developer.android.com/guide/topics/connectivity/localonlyhotspot) functionality.
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
This is only meant to be an index.
You can read more in the source code.
API restrictions are updated up to [SHA-256 checksum `7e00db074cbe51c51ff4b411f7b48e98692951395c5c17d069c822cc1d0eae0f`](https://github.com/Mygod/hiddenapi/commit/d339264309c2bbfed1379bacbd00387eeeb38896).

Greylisted/blacklisted APIs or internal constants: (some constants are hardcoded or implicitly used)

* (prior to API 30) `Landroid/net/ConnectivityManager;->getLastTetherError(Ljava/lang/String;)I,max-target-r`
* (since API 30) `Landroid/net/ConnectivityModuleConnector;->IN_PROCESS_SUFFIX:Ljava/lang/String;`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onTetherableInterfaceRegexpsChanged(Landroid/net/TetheringManager$TetheringInterfaceRegexps;)V,blocked`
* (since API 31) `Landroid/net/TetheringManager$TetheringEventCallback;->onSupportedTetheringTypes(Ljava/util/Set;)V,blocked`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->getCountryCode()Ljava/lang/String;,blocked`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setRandomizedMacAddress(Landroid/net/MacAddress;)Landroid/net/wifi/SoftApConfiguration$Builder;,blocked`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->BAND_TYPES:[I,blocked`
* (since API 31) `Landroid/net/wifi/SoftApInfo;->getApInstanceIdentifier()Ljava/lang/String;,blocked`
* (since API 31) `Landroid/net/wifi/WifiClient;->getApInstanceIdentifier()Ljava/lang/String;,blocked`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration$KeyMgmt;->FT_PSK:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration$KeyMgmt;->WPA_PSK_SHA256:I,blocked`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_2GHZ:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_5GHZ:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_ANY:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->apBand:I,unsupported`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->apChannel:I,unsupported`
* (prior to API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onNumClientsChanged(I)V,greylist-max-o`
* `Landroid/net/wifi/WifiManager;->cancelLocalOnlyHotspotRequest()V,unsupported`
* `Landroid/net/wifi/p2p/WifiP2pConfig$Builder;->MAC_ANY_ADDRESS:Landroid/net/MacAddress;,blocked`
* (since API 29) `Landroid/net/wifi/p2p/WifiP2pConfig$Builder;->mNetworkName:Ljava/lang/String;,blocked`
* `Landroid/net/wifi/p2p/WifiP2pManager;->startWps(Landroid/net/wifi/p2p/WifiP2pManager$Channel;Landroid/net/wifi/WpsInfo;Landroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,unsupported`
* (prior to API 30) `Landroid/provider/Settings$Global;->SOFT_AP_TIMEOUT_ENABLED:Ljava/lang/String;,lo-prio,max-target-o`
* (prior to API 30) `Lcom/android/internal/R$array;->config_tether_bluetooth_regexs:I,max-target-q`
* (prior to API 30) `Lcom/android/internal/R$array;->config_tether_usb_regexs:I,max-target-q`
* (prior to API 30) `Lcom/android/internal/R$array;->config_tether_wifi_regexs:I,max-target-q`
* (on API 29) `Lcom/android/internal/R$bool;->config_wifi_p2p_mac_randomization_supported:I,blacklist`
* (prior to API 30) `Lcom/android/internal/R$integer;->config_wifi_framework_soft_ap_timeout_delay:I,greylist-max-o`
* `Lcom/android/internal/R$string;->config_ethernet_iface_regex:I,lo-prio,max-target-o`
* (since API 30) `Lcom/android/server/wifi/WifiContext;->ACTION_RESOURCES_APK:Ljava/lang/String;`
* (since API 29) `Lcom/android/server/wifi/p2p/WifiP2pServiceImpl;->ANONYMIZED_DEVICE_ADDRESS:Ljava/lang/String;`
* (since API 30) `Lcom/android/server/SystemServer;->TETHERING_CONNECTOR_CLASS:Ljava/lang/String;`
* `Ljava/lang/invoke/MethodHandles$Lookup;-><init>(Ljava/lang/Class;I)V,unsupported`
* `Ljava/lang/invoke/MethodHandles$Lookup;->ALL_MODES:I,lo-prio,max-target-o`
* (prior to API 29) `Ljava/net/InetAddress;->parseNumericAddress(Ljava/lang/String;)Ljava/net/InetAddress;,core-platform-api,max-target-p`

<details>
<summary>Hidden whitelisted APIs: (same catch as above, however, things in this list are less likely to be broken)</summary>

* `Landroid/bluetooth/BluetoothPan;->isTetheringOn()Z,sdk,system-api,test-api`
* `Landroid/bluetooth/BluetoothProfile;->PAN:I,sdk,system-api,test-api`
* (since API 30) `Landroid/content/Context;->TETHERING_SERVICE:Ljava/lang/String;,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager$OnStartTetheringCallback;-><init>()V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager$OnStartTetheringCallback;->onTetheringFailed()V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager$OnStartTetheringCallback;->onTetheringStarted()V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager;->startTethering(IZLandroid/net/ConnectivityManager$OnStartTetheringCallback;Landroid/os/Handler;)V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager;->stopTethering(I)V,sdk,system-api,test-api`
* `Landroid/net/LinkProperties;->getAllInterfaceNames()Ljava/util/List;,sdk,system-api,test-api`
* `Landroid/net/LinkProperties;->getAllRoutes()Ljava/util/List;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$StartTetheringCallback;->onTetheringFailed(I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$StartTetheringCallback;->onTetheringStarted()V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onClientsChanged(Ljava/util/Collection;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onError(Ljava/lang/String;I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onOffloadStatusChanged(I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onTetherableInterfacesChanged(Ljava/util/List;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onTetheredInterfacesChanged(Ljava/util/List;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onTetheringSupported(Z)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onUpstreamChanged(Landroid/net/Network;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringRequest$Builder;-><init>(I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringRequest$Builder;->build()Landroid/net/TetheringManager$TetheringRequest;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringRequest$Builder;->setExemptFromEntitlementCheck(Z)Landroid/net/TetheringManager$TetheringRequest$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringRequest$Builder;->setShouldShowEntitlementUi(Z)Landroid/net/TetheringManager$TetheringRequest$Builder;,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->ACTION_TETHER_STATE_CHANGED:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->EXTRA_ACTIVE_LOCAL_ONLY:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->EXTRA_ACTIVE_TETHER:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->EXTRA_ERRORED_TETHER:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->TETHERING_BLUETOOTH:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHERING_ETHERNET:I,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->TETHERING_USB:I,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->TETHERING_WIFI:I,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->TETHER_ERROR_*:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_FAILED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_STARTED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_STOPPED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->registerTetheringEventCallback(Ljava/util/concurrent/Executor;Landroid/net/TetheringManager$TetheringEventCallback;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->startTethering(Landroid/net/TetheringManager$TetheringRequest;Ljava/util/concurrent/Executor;Landroid/net/TetheringManager$StartTetheringCallback;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->stopTethering(I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->unregisterTetheringEventCallback(Landroid/net/TetheringManager$TetheringEventCallback;)V,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_BAND_24G_SUPPORTED:J,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_BAND_5G_SUPPORTED:J,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_BAND_60G_SUPPORTED:J,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_BAND_6G_SUPPORTED:J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_*:J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApCapability;->areFeaturesSupported(J)Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApCapability;->getMaxSupportedClients()I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->getSupportedChannelList(I)[I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;-><init>()V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;-><init>(Landroid/net/wifi/SoftApConfiguration;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->build()Landroid/net/wifi/SoftApConfiguration;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAllowedAcsChannels(I[I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAllowedClientList(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAutoShutdownEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBand(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBlockedClientList(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBridgedModeOpportunisticShutdownEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBridgedModeOpportunisticShutdownTimeoutMillis(J)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBssid(Landroid/net/MacAddress;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setChannel(II)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setChannels(Landroid/util/SparseIntArray;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setClientControlByUserEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setHiddenSsid(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setIeee80211axEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setIeee80211beEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMacRandomizationSetting(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMaxChannelBandwidth(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMaxNumberOfClients(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setPassphrase(Ljava/lang/String;I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setShutdownTimeoutMillis(J)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30, prior to API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setSsid(Ljava/lang/String;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setVendorElements(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setWifiSsid(Landroid/net/wifi/WifiSsid;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->BAND_2GHZ:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->BAND_5GHZ:I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->BAND_60GHZ:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->BAND_6GHZ:I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->BAND_*:I,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->DEFAULT_TIMEOUT:J,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->RANDOMIZATION_NONE:I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->RANDOMIZATION_NON_PERSISTENT:I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->RANDOMIZATION_PERSISTENT:I,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->getAllowedAcsChannels(I)[I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->getAllowedClientList()Ljava/util/List;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->getBand()I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->getBlockedClientList()Ljava/util/List;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->getBridgedModeOpportunisticShutdownTimeoutMillis()J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->getChannel()I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->getChannels()Landroid/util/SparseIntArray;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->getMacRandomizationSetting()I,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->getMaxChannelBandwidth()I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->getMaxNumberOfClients()I,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->getPersistentRandomizedMacAddress()Landroid/net/MacAddress;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->getShutdownTimeoutMillis()J,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->getVendorElements()Ljava/util/List;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->isAutoShutdownEnabled()Z,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->isBridgedModeOpportunisticShutdownEnabled()Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->isClientControlByUserEnabled()Z,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->isIeee80211axEnabled()Z,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->isIeee80211beEnabled()Z,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->isUserConfiguration()Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApInfo;->CHANNEL_WIDTH_*:I,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApInfo;->CHANNEL_WIDTH_AUTO:I,sdk,system-api,test-api`
* (on API 30) `Landroid/net/wifi/SoftApInfo;->CHANNEL_WIDTH_INVALID:I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApInfo;->getAutoShutdownTimeoutMillis()J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApInfo;->getBandwidth()I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApInfo;->getBssid()Landroid/net/MacAddress;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApInfo;->getFrequency()I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApInfo;->getWifiStandard()I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiClient;->getMacAddress()Landroid/net/MacAddress;,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration$KeyMgmt;->WPA2_PSK:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onBlockedClientConnecting(Landroid/net/wifi/WifiClient;I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onCapabilityChanged(Landroid/net/wifi/SoftApCapability;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onConnectedClientsChanged(Ljava/util/List;)V,sdk,system-api,test-api`
* (on API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onInfoChanged(Landroid/net/wifi/SoftApInfo;)V,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/WifiManager$SoftApCallback;->onInfoChanged(Ljava/util/List;)V,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager$SoftApCallback;->onStateChanged(II)V,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->EXTRA_WIFI_AP_FAILURE_REASON:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->EXTRA_WIFI_AP_INTERFACE_NAME:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->EXTRA_WIFI_AP_STATE:Ljava/lang/String;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager;->SAP_CLIENT_BLOCK_REASON_CODE_*:I,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->SAP_START_FAILURE_*:I,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->WIFI_AP_STATE_CHANGED_ACTION:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->WIFI_AP_STATE_DISABLED:I,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->WIFI_AP_STATE_DISABLING:I,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->WIFI_AP_STATE_ENABLED:I,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->WIFI_AP_STATE_ENABLING:I,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->WIFI_AP_STATE_FAILED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager;->getSoftApConfiguration()Landroid/net/wifi/SoftApConfiguration;,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/wifi/WifiManager;->getWifiApConfiguration()Landroid/net/wifi/WifiConfiguration;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager;->isApMacRandomizationSupported()Z,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->registerSoftApCallback(Ljava/util/concurrent/Executor;Landroid/net/wifi/WifiManager$SoftApCallback;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager;->setSoftApConfiguration(Landroid/net/wifi/SoftApConfiguration;)Z,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/wifi/WifiManager;->setWifiApConfiguration(Landroid/net/wifi/WifiConfiguration;)Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager;->startLocalOnlyHotspot(Landroid/net/wifi/SoftApConfiguration;Ljava/util/concurrent/Executor;Landroid/net/wifi/WifiManager$LocalOnlyHotspotCallback;)V,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->unregisterSoftApCallback(Landroid/net/wifi/WifiManager$SoftApCallback;)V,sdk,system-api,test-api`
* `Landroid/net/wifi/p2p/WifiP2pGroupList;->getGroupList()Ljava/util/List;,sdk,system-api,test-api`
* `Landroid/net/wifi/p2p/WifiP2pManager$PersistentGroupInfoListener;->onPersistentGroupInfoAvailable(Landroid/net/wifi/p2p/WifiP2pGroupList;)V,sdk,system-api,test-api`
* `Landroid/net/wifi/p2p/WifiP2pManager;->deletePersistentGroup(Landroid/net/wifi/p2p/WifiP2pManager$Channel;ILandroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,sdk,system-api,test-api`
* `Landroid/net/wifi/p2p/WifiP2pManager;->requestPersistentGroupInfo(Landroid/net/wifi/p2p/WifiP2pManager$Channel;Landroid/net/wifi/p2p/WifiP2pManager$PersistentGroupInfoListener;)V,sdk,system-api,test-api`
* `Landroid/net/wifi/p2p/WifiP2pManager;->setWifiP2pChannels(Landroid/net/wifi/p2p/WifiP2pManager$Channel;IILandroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,sdk,system-api,test-api`
* `Landroid/provider/Settings$Global;->TETHER_OFFLOAD_DISABLED:Ljava/lang/String;,sdk,system-api,test-api`

</details>

Nonexported system resources:

* (since API 30) `@com.android.networkstack.tethering:array/config_tether_bluetooth_regexs`
* (since API 30) `@com.android.networkstack.tethering:array/config_tether_ncm_regexs`
* (since API 30) `@com.android.networkstack.tethering:array/config_tether_usb_regexs`
* (since API 30) `@com.android.networkstack.tethering:array/config_tether_wifi_p2p_regexs`
* (since API 30) `@com.android.networkstack.tethering:array/config_tether_wifi_regexs`
* (since API 30) `@com.android.networkstack.tethering:array/config_tether_wigig_regexs`
* (since API 30) `@com.android.wifi.resources:bool/config_wifi_p2p_mac_randomization_supported`
* (since API 31) `@com.android.wifi.resources:integer/config_wifiFrameworkSoftApShutDownIdleInstanceInBridgedModeTimeoutMillisecond`
* (since API 30) `@com.android.wifi.resources:integer/config_wifiFrameworkSoftApShutDownTimeoutMilliseconds`

Other: Activity `com.android.settings/.Settings$TetherSettingsActivity` is assumed to be exported.

For `ip rule` priorities, `RULE_PRIORITY_SECURE_VPN` and `RULE_PRIORITY_TETHERING` is assumed to be 12000 and 18000 respectively;
DHCP server like `dnsmasq` is assumed to run and send DHCP packets as root.

Undocumented system binaries are all bundled and executable:

* `iptables-save`, `ip6tables-save`;
* `echo`;
* `/system/bin/ip` (`monitor neigh rule unreachable`);
* `ndc` (`ipfwd nat`);
* `iptables`, `ip6tables` (with correct version corresponding to API level, `-nvx -L <chain>`);
* `sh`;
* `su`.

Wi-Fi driver `wpa_supplicant`:

* P2P configuration file is assumed to be saved to [`/data/vendor/wifi/wpa/p2p_supplicant.conf` or `/data/misc/wifi/p2p_supplicant.conf`](https://android.googlesource.com/platform/external/wpa_supplicant_8/+/0b4856b6dc451e290f1f64f6af17e010be78c073/wpa_supplicant/hidl/1.1/supplicant.cpp#26) and have reasonable format;
* Android system is expected to restart `wpa_supplicant` after it terminates.
