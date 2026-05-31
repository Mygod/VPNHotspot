# VPN Hotspot

[![Releases](https://img.shields.io/github/downloads/Mygod/VPNHotspot/total.svg)](https://github.com/Mygod/VPNHotspot/releases)
[![Android 10-16.1](https://img.shields.io/badge/Android-10--16.1-3DDC84?logo=android&logoColor=white)](mobile/build.gradle.kts)

[![Get it on Obtainium](https://github.com/ImranR98/Obtainium/raw/main/assets/graphics/badge_obtainium.png)](https://apps.obtainium.imranr.dev/redirect.html?r=obtainium%3A%2F%2Fapp%2F%257B%2522id%2522%253A%2522be.mygod.vpnhotspot%2522%252C%2522url%2522%253A%2522https%253A%252F%252Fgithub.com%252FMygod%252FVPNHotspot%2522%252C%2522author%2522%253A%2522%2522%252C%2522name%2522%253A%2522%2522%252C%2522additionalSettings%2522%253A%2522%257B%255C%2522includeZips%255C%2522%253Atrue%252C%255C%2522apkFilterRegEx%255C%2522%253A%255C%2522zip%255C%2522%257D%2522%257D)
([Pre-release channel](https://apps.obtainium.imranr.dev/redirect.html?r=obtainium%3A%2F%2Fapp%2F%257B%2522id%2522%253A%2522be.mygod.vpnhotspot%2522%252C%2522url%2522%253A%2522https%253A%252F%252Fgithub.com%252FMygod%252FVPNHotspot%2522%252C%2522author%2522%253A%2522%2522%252C%2522name%2522%253A%2522%2522%252C%2522additionalSettings%2522%253A%2522%257B%255C%2522includePrereleases%255C%2522%253Atrue%252C%255C%2522includeZips%255C%2522%253Atrue%252C%255C%2522apkFilterRegEx%255C%2522%253A%255C%2522zip%255C%2522%257D%2522%257D))

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

## Features That Requires System App Installation

The following features in the app requires it to be installed under `/system/priv-app` since some restricted permissions are required.
One way to do this is to use [App systemizer for Magisk](https://github.com/Magisk-Modules-Repo/terminal_systemizer).

* (Android 8-10, since app v2.4.0) `android.permission.OVERRIDE_WIFI_CONFIG`: Read/write system Wi-Fi hotspot configuration. ([#117](https://github.com/Mygod/VPNHotspot/issues/117))

Installing as system app also has the side benefit of launching root daemon less frequently due to having privileged permissions listed below.

* `android.permission.CONNECTIVITY_USE_RESTRICTED_NETWORKS`
* `android.permission.LOCAL_MAC_ADDRESS`
* `android.permission.MANAGE_USB`
* `android.permission.OVERRIDE_WIFI_CONFIG`
* `android.permission.READ_WIFI_CREDENTIAL`
* `android.permission.TETHER_PRIVILEGED`
* `android.permission.WRITE_SECURE_SETTINGS`
* Other system-app exclusive features that are not gated by permissions.

Whenever you install an app update, if there was a new protected permission addition (last updated in v2.17.1), you should update the app installed in system as well to make the system grant the privileged permission.

## Gateway sharing (single-arm router)

Besides sharing your VPN over a hotspot, this device can act as a gateway on an existing LAN it has joined as an ordinary client (Wi-Fi or Ethernet), forwarding other devices' traffic into the VPN. In networking terms this is a single-arm router (router-on-a-stick): the device routes traffic that arrives on the interface it is also using as a client.

This is more technical to set up than a hotspot, but lets you give a whole network VPN access without making clients connect to a separate hotspot.

To use it:

1. Root the device and connect it to the LAN as a normal client.
2. Connect a VPN on this device.
3. In the app's tethering screen, under **Gateway sharing**, enable the interface that holds the LAN address (e.g. `wlan0`).
4. On each other device, set its gateway **and DNS** to this device's IP on that LAN (statically, or via your router's DHCP). Their traffic then routes through the VPN; DNS is resolved over the VPN to avoid leaks.

## Q & A

Search the [issue tracker](https://github.com/Mygod/VPNHotspot/issues) for more.

### What changes exactly can this app do to my system? (and how to revert them)

1. If you change repeater credentials in unsafe mode, this feature will modify `/data/misc/wifi/p2p_supplicant.conf` or `/data/vendor/wifi/wpa/p2p_supplicant.conf`.
   If things stopped working after you use this feature, simply delete this file and everything should start working again.
2. If you edited the system Wi-Fi hotspot configuration through this app, those changes will also persist.
   Undo those changes if you face any issues.
3. If you toggle tethering hardware offload through this app, the Android global `tether_offload_disabled` setting will persist.
   Toggle it back in this app or the matching Developer options setting to revert it.

Routing, firewall, addresses, and daemon/service state managed by this app are cleaned up when stopped, by Clean, or upon reboot.

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
API restrictions are updated up to [SHA-256 checksum `9102af02fe6ab68b92464bdff5e5b09f3bd62c65d1130aaf85d3296f17d38074`](https://github.com/Mygod/hiddenapi/commit/2f90e9da30976febeb0630cba48c4da0116c323d).

Greylisted/blacklisted APIs or internal constants: (some constants are hardcoded or implicitly used)

* (prior to API 30) `Landroid/net/ConnectivityManager;->getLastTetherError(Ljava/lang/String;)I,max-target-r`
* (prior to API 30) `Landroid/net/ConnectivityManager;->EXTRA_ACTIVE_LOCAL_ONLY:Ljava/lang/String;,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/ConnectivityManager;->EXTRA_ACTIVE_TETHER:Ljava/lang/String;,max-target-r`
* (prior to API 30) `Landroid/net/ConnectivityManager;->EXTRA_AVAILABLE_TETHER:Ljava/lang/String;,max-target-r`
* (prior to API 30) `Landroid/net/ConnectivityManager;->ACTION_TETHER_STATE_CHANGED:Ljava/lang/String;,max-target-r`
* (prior to API 30) `Landroid/net/ConnectivityManager;->EXTRA_ERRORED_TETHER:Ljava/lang/String;,max-target-r`
* (since API 30) `Landroid/net/ConnectivityModuleConnector;->IN_PROCESS_SUFFIX:Ljava/lang/String;`
* (since API 31) `Landroid/net/INetd$Stub;->asInterface(Landroid/os/IBinder;)Landroid/net/INetd;`
* (since API 31) `Landroid/net/INetd;->ipSecUpdateSecurityPolicy(IIILjava/lang/String;Ljava/lang/String;IIII)V`
* (since API 30) `Landroid/net/IIntResultListener$Stub;-><init>()V,blocked`
* (since API 30) `Landroid/net/IIntResultListener;->onResult(I)V,blocked`
* (since API 30) `Landroid/net/ITetheringConnector;->stopTethering(ILjava/lang/String;Landroid/net/IIntResultListener;)V,blocked`
* (since API 30) `Landroid/net/ITetheringConnector;->stopTethering(ILjava/lang/String;Ljava/lang/String;Landroid/net/IIntResultListener;)V,blocked`
* (since API 30) `Landroid/net/TetheringManager$ConnectorConsumer;->onConnectorAvailable(Landroid/net/ITetheringConnector;)V,blocked`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onTetherableInterfaceRegexpsChanged(Landroid/net/TetheringManager$TetheringInterfaceRegexps;)V,blocked`
* (since API 31) `Landroid/net/TetheringManager$TetheringEventCallback;->onSupportedTetheringTypes(Ljava/util/Set;)V,blocked`
* (since API 30) `Landroid/net/TetheringManager;->getConnector(Landroid/net/TetheringManager$ConnectorConsumer;)V,blocked`
* `Landroid/net/TetheringManager;->TETHER_ERROR_*:I,blocked`
* (since API 30) `Landroid/net/TetheringManager;->TETHERING_VIRTUAL:I,blocked`
* (since API 31) `Landroid/net/IpSecManager;->DIRECTION_FWD:I,blocked`
* (since API 31) `Landroid/net/IpSecManager;->INVALID_SECURITY_PARAMETER_INDEX:I,blocked`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->getCountryCode()Ljava/lang/String;,blocked`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setRandomizedMacAddress(Landroid/net/MacAddress;)Landroid/net/wifi/SoftApConfiguration$Builder;,blocked`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->BAND_TYPES:[I,blocked`
* (since API 31) `Landroid/net/wifi/SoftApInfo;->getApInstanceIdentifier()Ljava/lang/String;,blocked`
* (since API 31) `Landroid/net/wifi/ISoftApCallback$Stub;->asInterface(Landroid/os/IBinder;)Landroid/net/wifi/ISoftApCallback;,lo-prio,max-target-o`
* (since API 33) `Landroid/net/wifi/IWifiManager;->registerLocalOnlyHotspotSoftApCallback(Landroid/net/wifi/ISoftApCallback;Landroid/os/Bundle;)V,blocked`
* (since API 31) `Landroid/net/wifi/IWifiManager;->registerSoftApCallback(Landroid/net/wifi/ISoftApCallback;)V,blocked`
* (since API 33) `Landroid/net/wifi/IWifiManager;->unregisterLocalOnlyHotspotSoftApCallback(Landroid/net/wifi/ISoftApCallback;Landroid/os/Bundle;)V,blocked`
* (since API 31) `Landroid/net/wifi/IWifiManager;->unregisterSoftApCallback(Landroid/net/wifi/ISoftApCallback;)V,blocked`
* (since API 31) `Landroid/net/wifi/WifiClient;->getApInstanceIdentifier()Ljava/lang/String;,blocked`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration$KeyMgmt;->FT_PSK:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration$KeyMgmt;->WPA_PSK_SHA256:I,blocked`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_2GHZ:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_5GHZ:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_ANY:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->apBand:I,unsupported`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->apChannel:I,unsupported`
* (since API 30) `Landroid/net/wifi/WifiContext;->ACTION_RESOURCES_APK:Ljava/lang/String;,blocked`
* (since API 31, prior to API 33) `Landroid/net/wifi/WifiManager$SoftApCallbackProxy;-><init>(Landroid/net/wifi/WifiManager;Ljava/util/concurrent/Executor;Landroid/net/wifi/WifiManager$SoftApCallback;)V,blocked`
* (since API 33) `Landroid/net/wifi/WifiManager$SoftApCallbackProxy;-><init>(Landroid/net/wifi/WifiManager;Ljava/util/concurrent/Executor;Landroid/net/wifi/WifiManager$SoftApCallback;I)V,blocked`
* (prior to API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onNumClientsChanged(I)V,greylist-max-o`
* (since API 33) `Landroid/net/wifi/WifiManager;->EXTRA_PARAM_KEY_ATTRIBUTION_SOURCE:Ljava/lang/String;,blocked`
* `Landroid/net/wifi/WifiManager;->cancelLocalOnlyHotspotRequest()V,unsupported`
* (since API 31) `Landroid/net/wifi/WifiManager;->mService:Landroid/net/wifi/IWifiManager;,unsupported`
* `Landroid/net/wifi/p2p/WifiP2pConfig$Builder;->MAC_ANY_ADDRESS:Landroid/net/MacAddress;,blocked`
* `Landroid/net/wifi/p2p/WifiP2pConfig$Builder;->mNetworkName:Ljava/lang/String;,blocked`
* (since API 30) `Landroid/net/wifi/p2p/WifiP2pGroup;->interfaceAddress:[B,unsupported`
* `Landroid/net/wifi/p2p/WifiP2pManager;->startWps(Landroid/net/wifi/p2p/WifiP2pManager$Channel;Landroid/net/wifi/WpsInfo;Landroid/net/wifi/p2p/WifiP2pManager$ActionListener;)V,unsupported`
* (prior to API 30) `Landroid/provider/Settings$Global;->SOFT_AP_TIMEOUT_ENABLED:Ljava/lang/String;,lo-prio,max-target-o`
* (on API 34) `Landroid/service/quicksettings/TileService;->mToken:Landroid/os/IBinder;,lo-prio,max-target-o`
* (prior to API 30) `Lcom/android/internal/R$array;->config_tether_bluetooth_regexs:I,max-target-q`
* (prior to API 30) `Lcom/android/internal/R$array;->config_tether_usb_regexs:I,max-target-q`
* (prior to API 30) `Lcom/android/internal/R$array;->config_tether_wifi_regexs:I,max-target-q`
* (on API 29) `Lcom/android/internal/R$bool;->config_wifi_p2p_mac_randomization_supported:I,blacklist`
* (prior to API 30) `Lcom/android/internal/R$integer;->config_wifi_framework_soft_ap_timeout_delay:I,greylist-max-o`
* `Lcom/android/internal/R$string;->config_ethernet_iface_regex:I,lo-prio,max-target-o`
* `Lcom/android/server/wifi/p2p/WifiP2pServiceImpl;->ANONYMIZED_DEVICE_ADDRESS:Ljava/lang/String;`
* (since API 30) `Lcom/android/server/SystemServer;->TETHERING_CONNECTOR_CLASS:Ljava/lang/String;`
* (since API 33) `Ldalvik/system/BaseDexClassLoader;->pathList:Ldalvik/system/DexPathList;,unsupported`
* (since API 33) `Ldalvik/system/DexPathList;->nativeLibraryDirectories:Ljava/util/List;,unsupported`
* (prior to API 33) `Ljava/lang/invoke/MethodHandles$Lookup;-><init>(Ljava/lang/Class;I)V,unsupported`
* (prior to API 33) `Ljava/lang/invoke/MethodHandles$Lookup;->ALL_MODES:I,lo-prio,max-target-o`

See [`mobile/src/hiddenApiStubs`](mobile/src/hiddenApiStubs) for hidden whitelisted/system APIs as well as partial SDK-class stubs.

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

Other:

* (prior to API 30) Activity `com.android.settings/.Settings$TetherSettingsActivity` is assumed to be exported.
* `IPv6 NAT` mode depends on the iptables `TPROXY` and `NFQUEUE` targets and
  transparent sockets. ICMPv6 Echo interception uses app-owned queue `30000`
  and assumes queued downstream packets expose six-byte source hardware-address
  metadata through `NFQA_HWADDR`.
* (since API 30) Relevant tethering APEX classes used here, including `android.net.ITetheringConnector`,
  may be jarjar-relocated under the optional prefixes
  `android.net.connectivity` or `com.android.connectivity`.
* (since API 31) Relevant netd APEX classes used here, including `android.net.INetd*`,
  may be jarjar-relocated under the optional prefixes
  `android.net.connectivity` or `com.android.connectivity`.
* (since API 30) When runtime `TetheringEventCallback.onLocalOnlyInterfacesChanged` is present, AOSP dispatches
  startup tether-state callbacks from one `executor.execute { ... }` block in `onCallbackStarted`,
  and later tether-state updates from one `executor.execute { ... }` block in
  `onTetherStatesChanged`.
* The Rust DNS proxy submits upstream queries through `android_res_nsend`/`android_res_nresult`.
  To keep daemon tasks nonblocking while still using `android_res_nresult` as the public result
  reader/closer, it waits for `dnsproxyd` to close the one-shot `resnsend` client socket before
  reading the result. This assumes `resnsend` writes the complete resolver result before returning
  and the socket receive buffer can hold that result until the framework socket listener closes the
  client socket.
* For `ip rule` priorities, AOSP local-network/tethering priorities are assumed to be 17000/18000
  on API 29..30 and 20000/21000 on API 31+. VPNHotspot uses the 17500..17900 or 20500..20900
  gap between them.
* For route-table numbers, Android interface tables are assumed to start at ifindex + 1000; `IPv6 NAT`
  TPROXY uses table 900 to stay below that range and away from AOSP fixed tables 97..99 and kernel built-ins.
* Clean flushes table 900 because that table is reserved by VPNHotspot. `IPv6 NAT` also adds its
  deterministic ULA /64 route to Android's shared `local_network` table; Clean never flushes that table
  and only deletes VPNHotspot prefixes reconstructed from current interface names.
* For packet marks, Android fwmark is assumed to use low bits for netId and routing metadata.
* `IPv6 NAT` fwmark fallback for TPROXY uses masked high reserved bits `0x10000000/0x10000000`.
  That fallback is expected on only kernels without effective `FRA_IP_PROTO` policy-rule support, which upstream Linux added in 4.17.
* Daemon reply sockets use the AOSP local-network protected mark `0x00030063`, which assumes
  `LOCAL_NET_ID = 99` plus the `explicitlySelected` and `protectedFromVpn` fwmark bits.

System/root command assumptions:

The following Android system binaries are assumed to be bundled and executable:

* `/system/bin/dumpsys` (`ipsec`);
* `/system/bin/iptables-restore`, `/system/bin/ip6tables-restore` (`-w --noflush`, restore input
  commands including `-I`, `-D`, `-N`, `-nvx -L <chain>`);
* `/system/bin/ndc` (`ipfwd`, `nat`);
* `/system/bin/settings` (`put global`);
* `/system/bin/linker` or `/system/bin/linker64` (`path.zip!/program`).

Wi-Fi driver `wpa_supplicant`:

* P2P configuration file is assumed to be saved to [`/data/vendor/wifi/wpa/p2p_supplicant.conf` or `/data/misc/wifi/p2p_supplicant.conf`](https://android.googlesource.com/platform/external/wpa_supplicant_8/+/0b4856b6dc451e290f1f64f6af17e010be78c073/wpa_supplicant/hidl/1.1/supplicant.cpp#26) and have reasonable format;
* Android system is expected to restart `wpa_supplicant` after it terminates.
