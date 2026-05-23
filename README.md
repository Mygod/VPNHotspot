# VPN Hotspot

[![Test](https://github.com/Mygod/VPNHotspot/actions/workflows/test.yml/badge.svg)](https://github.com/Mygod/VPNHotspot/actions/workflows/test.yml)
[![Releases](https://img.shields.io/github/downloads/Mygod/VPNHotspot/total.svg)](https://github.com/Mygod/VPNHotspot/releases)
[![Android 10-16.1](https://img.shields.io/badge/Android-10--16.1-3DDC84?logo=android&logoColor=white)](mobile/build.gradle.kts)

[![Get it on Obtainium](https://github.com/ImranR98/Obtainium/raw/main/assets/graphics/badge_obtainium.png)](https://apps.obtainium.imranr.dev/redirect?r=obtainium://add/https://github.com/Mygod/VPNHotspot)

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
* (since API 31) `Landroid/net/wifi/WifiClient;->getApInstanceIdentifier()Ljava/lang/String;,blocked`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration$KeyMgmt;->FT_PSK:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration$KeyMgmt;->WPA_PSK_SHA256:I,blocked`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_2GHZ:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_5GHZ:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->AP_BAND_ANY:I,lo-prio,max-target-o`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->apBand:I,unsupported`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration;->apChannel:I,unsupported`
* (since API 30) `Landroid/net/wifi/WifiContext;->ACTION_RESOURCES_APK:Ljava/lang/String;,blocked`
* (prior to API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onNumClientsChanged(I)V,greylist-max-o`
* `Landroid/net/wifi/WifiManager;->cancelLocalOnlyHotspotRequest()V,unsupported`
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
<details>
<summary>Hidden whitelisted APIs: (same catch as above, however, things in this list are less likely to be broken)</summary>

* `Landroid/bluetooth/BluetoothPan;->isTetheringOn()Z,sdk,system-api,test-api`
* `Landroid/bluetooth/BluetoothProfile;->PAN:I,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager$OnStartTetheringCallback;-><init>()V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager$OnStartTetheringCallback;->onTetheringFailed()V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager$OnStartTetheringCallback;->onTetheringStarted()V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager;->startTethering(IZLandroid/net/ConnectivityManager$OnStartTetheringCallback;Landroid/os/Handler;)V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager;->stopTethering(I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/LinkAddress;->getDeprecationTime()J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/LinkAddress;->getExpirationTime()J,sdk,system-api,test-api`
* `Landroid/net/LinkProperties;->getAllInterfaceNames()Ljava/util/List;,sdk,system-api,test-api`
* `Landroid/net/LinkProperties;->getAllRoutes()Ljava/util/List;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheredClient$AddressInfo;->getAddress()Landroid/net/LinkAddress;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheredClient$AddressInfo;->getHostname()Ljava/lang/String;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheredClient;->getAddresses()Ljava/util/List;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheredClient;->getMacAddress()Landroid/net/MacAddress;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheredClient;->getTetheringType()I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onClientsChanged(Ljava/util/Collection;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onError(Ljava/lang/String;I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onError(Landroid/net/TetheringInterface;I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onLocalOnlyInterfacesChanged(Ljava/util/Set;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onOffloadStatusChanged(I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onTetherableInterfacesChanged(Ljava/util/List;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onTetherableInterfacesChanged(Ljava/util/Set;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onTetheringSupported(Z)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringEventCallback;->onUpstreamChanged(Landroid/net/Network;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringRequest$Builder;->setExemptFromEntitlementCheck(Z)Landroid/net/TetheringManager$TetheringRequest$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager$TetheringRequest$Builder;->setShouldShowEntitlementUi(Z)Landroid/net/TetheringManager$TetheringRequest$Builder;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/TetheringManager;->ACTION_TETHER_STATE_CHANGED:Ljava/lang/String;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/TetheringManager;->EXTRA_ACTIVE_LOCAL_ONLY:Ljava/lang/String;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/TetheringManager;->EXTRA_ACTIVE_TETHER:Ljava/lang/String;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/TetheringManager;->EXTRA_AVAILABLE_TETHER:Ljava/lang/String;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/TetheringManager;->EXTRA_ERRORED_TETHER:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->TETHERING_BLUETOOTH:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHERING_ETHERNET:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHERING_NCM:I,sdk,system-api,test-api`
* `Landroid/net/TetheringManager;->TETHERING_USB:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHERING_WIFI_P2P:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHERING_WIGIG:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_FAILED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_STARTED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_STOPPED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/DeauthenticationReasonCode;->REASON_*:I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_BAND_24G_SUPPORTED:J,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_BAND_5G_SUPPORTED:J,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_BAND_60G_SUPPORTED:J,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_BAND_6G_SUPPORTED:J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_*:J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApCapability;->areFeaturesSupported(J)Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApCapability;->getMaxSupportedClients()I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApCapability;->getSupportedChannelList(I)[I,sdk,system-api,test-api`
* (since API 35) `Landroid/net/wifi/OuiKeyedData$Builder;-><init>(ILandroid/os/PersistableBundle;)V,sdk,system-api,test-api`
* (since API 35) `Landroid/net/wifi/OuiKeyedData$Builder;->build()Landroid/net/wifi/OuiKeyedData;,sdk,system-api,test-api`
* (since API 35) `Landroid/net/wifi/OuiKeyedData;->getData()Landroid/os/PersistableBundle;,sdk,system-api,test-api`
* (since API 35) `Landroid/net/wifi/OuiKeyedData;->getOui()I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;-><init>(Landroid/net/wifi/SoftApConfiguration;)V,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAllowedAcsChannels(I[I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAllowedClientList(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAutoShutdownEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBand(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBandOptimizationEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBlockedClientList(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBridgedModeOpportunisticShutdownEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBridgedModeOpportunisticShutdownTimeoutMillis(J)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBssid(Landroid/net/MacAddress;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setChannel(II)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setClientControlByUserEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 36) `Landroid/net/wifi/SoftApConfiguration$Builder;->setClientIsolationEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setHiddenSsid(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setIeee80211axEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setIeee80211beEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMacRandomizationSetting(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMaxChannelBandwidth(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMaxNumberOfClients(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setPassphrase(Ljava/lang/String;I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setShutdownTimeoutMillis(J)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30, prior to API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setSsid(Ljava/lang/String;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 35) `Landroid/net/wifi/SoftApConfiguration$Builder;->setVendorData(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
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
* (since API 35) `Landroid/net/wifi/SoftApConfiguration;->getVendorData()Ljava/util/List;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->getVendorElements()Ljava/util/List;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->isAutoShutdownEnabled()Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->isBandOptimizationEnabled()Z`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->isBridgedModeOpportunisticShutdownEnabled()Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->isClientControlByUserEnabled()Z,sdk,system-api,test-api`
* (since API 36) `Landroid/net/wifi/SoftApConfiguration;->isClientIsolationEnabled()Z,sdk,system-api,test-api`
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
* (since API 30) `Landroid/net/wifi/SoftApInfo;->getMldAddress()Landroid/net/MacAddress;,sdk,system-api,test-api`
* (since API 35) `Landroid/net/wifi/SoftApInfo;->getVendorData()Ljava/util/List;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApInfo;->getWifiStandard()I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiClient;->getDisconnectReason()I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiClient;->getMacAddress()Landroid/net/MacAddress;,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/wifi/WifiConfiguration$KeyMgmt;->WPA2_PSK:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onBlockedClientConnecting(Landroid/net/wifi/WifiClient;I)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onCapabilityChanged(Landroid/net/wifi/SoftApCapability;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager$SoftApCallback;->onClientsDisconnected(Landroid/net/wifi/SoftApInfo;Ljava/util/List;)V,sdk,system-api,test-api`
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

Other:

* Activity `com.android.settings/.Settings$TetherSettingsActivity` is assumed to be exported.
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
* `/system/bin/linker`, `/system/bin/linker64` (`path.zip!/program`).

Wi-Fi driver `wpa_supplicant`:

* P2P configuration file is assumed to be saved to [`/data/vendor/wifi/wpa/p2p_supplicant.conf` or `/data/misc/wifi/p2p_supplicant.conf`](https://android.googlesource.com/platform/external/wpa_supplicant_8/+/0b4856b6dc451e290f1f64f6af17e010be78c073/wpa_supplicant/hidl/1.1/supplicant.cpp#26) and have reasonable format;
* Android system is expected to restart `wpa_supplicant` after it terminates.
