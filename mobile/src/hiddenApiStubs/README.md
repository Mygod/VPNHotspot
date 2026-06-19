# Hidden API stubs

This directory is the central audit point for framework APIs that need compile assistance or hidden-API tracking.

`java/` is compiled into a `compileOnly` stub jar.
Keep that jar limited to framework classes, or binary-name nested classes, that are missing from SDK stubs and are directly referenced by app code.
Use abstract class if we require a class to implement or extend a base interface or class.
Do not add stubs for private nested framework interfaces such as `TetheringManager$ConnectorConsumer`: direct linking still fails runtime Java access checks, so those must stay behind `Class.forName` and `Proxy`.
Stub declarations should carry `androidx.annotation.RequiresApi` for the exact platform introduction point when useful, and always when that point is above this app's minSdk.
Use declaration comments in the stub only for the API levels where VPNHotspot actually uses the member, especially when that differs from the introduction point or depends on a runtime probe.

APIs whose owner class is already in `android.jar` should stay as direct SDK usage, reflection, or hardcoded constants at the call site.
These additional reflected whitelisted/system API are listed below:

* `Landroid/bluetooth/BluetoothProfile;->PAN:I,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager;->startTethering(IZLandroid/net/ConnectivityManager$OnStartTetheringCallback;Landroid/os/Handler;)V,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/ConnectivityManager;->stopTethering(I)V,sdk,system-api,test-api`
* `Landroid/net/LinkAddress;-><init>(Ljava/lang/String;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/LinkAddress;->getDeprecationTime()J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/LinkAddress;->getExpirationTime()J,sdk,system-api,test-api`
* `Landroid/net/LinkProperties;->getAllInterfaceNames()Ljava/util/List;,sdk,system-api,test-api`
* `Landroid/net/LinkProperties;->getAllRoutes()Ljava/util/List;,sdk,system-api,test-api`
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
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_FAILED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_STARTED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/TetheringManager;->TETHER_HARDWARE_OFFLOAD_STOPPED:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/DeauthenticationReasonCode;->REASON_*:I,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApCapability;->SOFTAP_FEATURE_*:J,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;-><init>(Landroid/net/wifi/SoftApConfiguration;)V,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAllowedAcsChannels(I[I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAllowedClientList(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setAutoShutdownEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBand(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30; source-backed system API, no row in the checked hiddenapi CSV) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBandOptimizationEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBlockedClientList(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBridgedModeOpportunisticShutdownEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setBridgedModeOpportunisticShutdownTimeoutMillis(J)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (on API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setChannel(II)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setClientControlByUserEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 36) `Landroid/net/wifi/SoftApConfiguration$Builder;->setClientIsolationEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setHiddenSsid(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setIeee80211axEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setIeee80211beEnabled(Z)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMacRandomizationSetting(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMaxChannelBandwidth(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setMaxNumberOfClients(I)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration$Builder;->setShutdownTimeoutMillis(J)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 30, prior to API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setSsid(Ljava/lang/String;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 35) `Landroid/net/wifi/SoftApConfiguration$Builder;->setVendorData(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration$Builder;->setVendorElements(Ljava/util/List;)Landroid/net/wifi/SoftApConfiguration$Builder;,sdk,system-api,test-api`
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
* (since API 30; source-backed system API, no row in the checked hiddenapi CSV) `Landroid/net/wifi/SoftApConfiguration;->isBandOptimizationEnabled()Z`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->isBridgedModeOpportunisticShutdownEnabled()Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApConfiguration;->isClientControlByUserEnabled()Z,sdk,system-api,test-api`
* (since API 36) `Landroid/net/wifi/SoftApConfiguration;->isClientIsolationEnabled()Z,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->isIeee80211axEnabled()Z,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/SoftApConfiguration;->isIeee80211beEnabled()Z,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/SoftApConfiguration;->isUserConfiguration()Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/SoftApInfo;->CHANNEL_WIDTH_*:I,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->EXTRA_WIFI_AP_FAILURE_REASON:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->EXTRA_WIFI_AP_INTERFACE_NAME:Ljava/lang/String;,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->EXTRA_WIFI_AP_STATE:Ljava/lang/String;,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/WifiManager;->IFACE_IP_MODE_LOCAL_ONLY:I,sdk,system-api,test-api`
* (since API 31) `Landroid/net/wifi/WifiManager;->IFACE_IP_MODE_TETHERED:I,sdk,system-api,test-api`
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
* (since API 33) `Landroid/net/wifi/WifiManager;->registerLocalOnlyHotspotSoftApCallback(Ljava/util/concurrent/Executor;Landroid/net/wifi/WifiManager$SoftApCallback;)V,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->registerSoftApCallback(Ljava/util/concurrent/Executor;Landroid/net/wifi/WifiManager$SoftApCallback;)V,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager;->setSoftApConfiguration(Landroid/net/wifi/SoftApConfiguration;)Z,sdk,system-api,test-api`
* (prior to API 30) `Landroid/net/wifi/WifiManager;->setWifiApConfiguration(Landroid/net/wifi/WifiConfiguration;)Z,sdk,system-api,test-api`
* (since API 30) `Landroid/net/wifi/WifiManager;->startLocalOnlyHotspot(Landroid/net/wifi/SoftApConfiguration;Ljava/util/concurrent/Executor;Landroid/net/wifi/WifiManager$LocalOnlyHotspotCallback;)V,sdk,system-api,test-api`
* (since API 33) `Landroid/net/wifi/WifiManager;->unregisterLocalOnlyHotspotSoftApCallback(Landroid/net/wifi/WifiManager$SoftApCallback;)V,sdk,system-api,test-api`
* `Landroid/net/wifi/WifiManager;->unregisterSoftApCallback(Landroid/net/wifi/WifiManager$SoftApCallback;)V,sdk,system-api,test-api`
* (since API 35) `Landroid/net/wifi/p2p/WifiP2pGroup;->getVendorData()Ljava/util/List;,sdk,system-api,test-api`
* `Landroid/net/wifi/p2p/WifiP2pManager;->requestPersistentGroupInfo(Landroid/net/wifi/p2p/WifiP2pManager$Channel;Landroid/net/wifi/p2p/WifiP2pManager$PersistentGroupInfoListener;)V,sdk,system-api,test-api`
* `Landroid/provider/Settings$Global;->TETHER_OFFLOAD_DISABLED:Ljava/lang/String;,sdk,system-api,test-api`
* (since API 30) `Landroid/provider/Settings;->ACTION_TETHER_SETTINGS:Ljava/lang/String;,sdk,system-api,test-api`
