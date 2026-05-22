package be.mygod.vpnhotspot.ui

import android.content.Context
import android.icu.text.DecimalFormat
import android.icu.text.DecimalFormatSymbols
import android.net.TetheringManager
import android.net.wifi.SoftApConfiguration
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.wifi.SoftApCapability
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.SoftApInfo
import be.mygod.vpnhotspot.net.wifi.WifiApManager

fun channelBandwidthLabel(
    context: Context,
    width: Int,
    fallback: String = SoftApInfo.channelWidthLookup(width, true),
) = when (width) {
    SoftApConfigurationCompat.CHANNEL_WIDTH_AUTO -> context.getString(R.string.wifi_channel_width_auto)
    SoftApConfigurationCompat.CHANNEL_WIDTH_INVALID -> context.getString(R.string.wifi_channel_width_invalid)
    1 -> context.getString(R.string.wifi_channel_width_20mhz_no_ht)
    2 -> context.getString(R.string.wifi_channel_width_mhz, 20)
    3 -> context.getString(R.string.wifi_channel_width_mhz, 40)
    4 -> context.getString(R.string.wifi_channel_width_mhz, 80)
    5 -> context.getString(R.string.wifi_channel_width_160mhz_80_plus_80)
    6 -> context.getString(R.string.wifi_channel_width_mhz, 160)
    7 -> context.getString(R.string.wifi_channel_width_mhz, 2160)
    8 -> context.getString(R.string.wifi_channel_width_mhz, 4320)
    9 -> context.getString(R.string.wifi_channel_width_mhz, 6480)
    10 -> context.getString(R.string.wifi_channel_width_mhz, 8640)
    11 -> context.getString(R.string.wifi_channel_width_mhz, 320)
    else -> fallback
}

fun softApBandLabel(
    context: Context,
    band: Int,
    fallback: String = SoftApConfigurationCompat.bandLookup(band, true),
): String {
    val format = DecimalFormat("#.#", DecimalFormatSymbols.getInstance(context.resources.configuration.locales[0]))
    val bands = arrayOf(
        SoftApConfiguration.BAND_2GHZ to 2.4,
        SoftApConfiguration.BAND_5GHZ to 5.0,
        SoftApConfiguration.BAND_6GHZ to 6.0,
        SoftApConfiguration.BAND_60GHZ to 60.0,
    )
    val knownBands = bands.fold(0) { acc, (mask, _) -> acc or mask }
    if (band and knownBands != band) return fallback
    val bandNames = bands.filter { (mask, _) -> band and mask == mask }
    return if (bandNames.isEmpty()) fallback else bandNames.joinToString("/") { (_, name) ->
        context.getString(R.string.wifi_ap_choose_G_short, format.format(name))
    }
}

fun tetherErrorLabel(
    context: Context,
    error: Int,
    fallback: String = TetheringManagerCompat.tetherErrorLookup(error),
) = when (error) {
    TetheringManager.TETHER_ERROR_NO_ERROR -> context.getString(R.string.tether_error_no_error)
    TetheringManager.TETHER_ERROR_UNKNOWN_IFACE -> context.getString(R.string.tether_error_unknown_iface)
    TetheringManager.TETHER_ERROR_SERVICE_UNAVAIL -> context.getString(R.string.tether_error_service_unavailable)
    TetheringManager.TETHER_ERROR_UNSUPPORTED -> context.getString(R.string.tether_error_unsupported)
    TetheringManager.TETHER_ERROR_UNAVAIL_IFACE -> context.getString(R.string.tether_error_unavailable_iface)
    TetheringManager.TETHER_ERROR_INTERNAL_ERROR -> context.getString(R.string.tether_error_internal)
    TetheringManager.TETHER_ERROR_TETHER_IFACE_ERROR -> context.getString(R.string.tether_error_tether_iface)
    TetheringManager.TETHER_ERROR_UNTETHER_IFACE_ERROR -> context.getString(R.string.tether_error_untether_iface)
    TetheringManager.TETHER_ERROR_ENABLE_FORWARDING_ERROR -> context.getString(R.string.tether_error_enable_forwarding)
    TetheringManager.TETHER_ERROR_DISABLE_FORWARDING_ERROR -> context.getString(R.string.tether_error_disable_forwarding)
    TetheringManager.TETHER_ERROR_IFACE_CFG_ERROR -> context.getString(R.string.tether_error_iface_config)
    TetheringManager.TETHER_ERROR_PROVISIONING_FAILED -> context.getString(R.string.tether_error_provisioning_failed)
    TetheringManager.TETHER_ERROR_DHCPSERVER_ERROR -> context.getString(R.string.tether_error_dhcp_server)
    TetheringManager.TETHER_ERROR_ENTITLEMENT_UNKNOWN -> context.getString(R.string.tether_error_entitlement_unknown)
    TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION ->
        context.getString(R.string.tether_error_no_change_permission)
    TetheringManager.TETHER_ERROR_NO_ACCESS_TETHERING_PERMISSION ->
        context.getString(R.string.tether_error_no_access_permission)
    TetheringManager.TETHER_ERROR_UNKNOWN_TYPE -> context.getString(R.string.tether_error_unknown_type)
    TetheringManager.TETHER_ERROR_UNKNOWN_REQUEST -> context.getString(R.string.tether_error_unknown_request)
    TetheringManager.TETHER_ERROR_DUPLICATE_REQUEST -> context.getString(R.string.tether_error_duplicate_request)
    else -> fallback
}

fun softApStartFailureLabel(
    context: Context,
    reason: Int,
    fallback: String = WifiApManager.failureReasonLookup(reason),
) = when (reason) {
    0 -> context.getString(R.string.softap_start_failure_general)
    1 -> context.getString(R.string.softap_start_failure_no_channel)
    2 -> context.getString(R.string.softap_start_failure_unsupported_configuration)
    3 -> context.getString(R.string.softap_start_failure_user_rejected)
    else -> fallback
}

fun softApClientBlockReasonLabel(
    context: Context,
    reason: Int,
    fallback: String = WifiApManager.clientBlockLookup(reason, true),
) = when (reason) {
    0 -> context.getString(R.string.softap_client_blocked_by_user)
    1 -> context.getString(R.string.softap_client_blocked_no_more_stas)
    else -> fallback
}

fun softApClientDisconnectReasonLabel(
    context: Context,
    reason: Int,
    fallback: String = WifiApManager.deauthenticationReasonLookup(reason, true),
) = when (reason) {
    0 -> context.getString(R.string.softap_disconnect_reason_unknown)
    1 -> context.getString(R.string.softap_disconnect_reason_unspecified)
    2 -> context.getString(R.string.softap_disconnect_reason_previous_auth_not_valid)
    3 -> context.getString(R.string.softap_disconnect_reason_deauth_leaving)
    4 -> context.getString(R.string.softap_disconnect_reason_inactivity)
    5 -> context.getString(R.string.softap_disconnect_reason_ap_busy)
    6 -> context.getString(R.string.softap_disconnect_reason_class2_frame_from_non_auth)
    7 -> context.getString(R.string.softap_disconnect_reason_class3_frame_from_non_assoc)
    8 -> context.getString(R.string.softap_disconnect_reason_sta_left)
    9 -> context.getString(R.string.softap_disconnect_reason_assoc_without_auth)
    10 -> context.getString(R.string.softap_disconnect_reason_power_capability_not_valid)
    11 -> context.getString(R.string.softap_disconnect_reason_supported_channel_not_valid)
    12 -> context.getString(R.string.softap_disconnect_reason_bss_transition)
    13 -> context.getString(R.string.softap_disconnect_reason_invalid_ie)
    14 -> context.getString(R.string.softap_disconnect_reason_michael_mic_failure)
    15 -> context.getString(R.string.softap_disconnect_reason_four_way_handshake_timeout)
    16 -> context.getString(R.string.softap_disconnect_reason_group_key_update_timeout)
    17 -> context.getString(R.string.softap_disconnect_reason_ie_in_4way_differs)
    18 -> context.getString(R.string.softap_disconnect_reason_group_cipher_not_valid)
    19 -> context.getString(R.string.softap_disconnect_reason_pairwise_cipher_not_valid)
    20 -> context.getString(R.string.softap_disconnect_reason_akmp_not_valid)
    21 -> context.getString(R.string.softap_disconnect_reason_unsupported_rsn_ie_version)
    22 -> context.getString(R.string.softap_disconnect_reason_invalid_rsn_ie_capabilities)
    23 -> context.getString(R.string.softap_disconnect_reason_ieee8021x_auth_failed)
    24 -> context.getString(R.string.softap_disconnect_reason_cipher_suite_rejected)
    25 -> context.getString(R.string.softap_disconnect_reason_tdls_teardown_unreachable)
    26 -> context.getString(R.string.softap_disconnect_reason_tdls_teardown_unspecified)
    27 -> context.getString(R.string.softap_disconnect_reason_ssp_requested_disassoc)
    28 -> context.getString(R.string.softap_disconnect_reason_no_ssp_roaming_agreement)
    29 -> context.getString(R.string.softap_disconnect_reason_bad_cipher_or_akm)
    30 -> context.getString(R.string.softap_disconnect_reason_not_authorized_location)
    31 -> context.getString(R.string.softap_disconnect_reason_service_change_precludes_ts)
    32 -> context.getString(R.string.softap_disconnect_reason_unspecified_qos)
    33 -> context.getString(R.string.softap_disconnect_reason_not_enough_bandwidth)
    34 -> context.getString(R.string.softap_disconnect_reason_low_ack)
    35 -> context.getString(R.string.softap_disconnect_reason_exceeded_txop)
    36 -> context.getString(R.string.softap_disconnect_reason_sta_leaving)
    37 -> context.getString(R.string.softap_disconnect_reason_end_ts_ba_dls)
    38 -> context.getString(R.string.softap_disconnect_reason_unknown_ts_ba)
    39 -> context.getString(R.string.softap_disconnect_reason_timeout)
    45 -> context.getString(R.string.softap_disconnect_reason_peerkey_mismatch)
    46 -> context.getString(R.string.softap_disconnect_reason_authorized_access_limit_reached)
    47 -> context.getString(R.string.softap_disconnect_reason_external_service_requirements)
    48 -> context.getString(R.string.softap_disconnect_reason_invalid_ft_action_frame_count)
    49 -> context.getString(R.string.softap_disconnect_reason_invalid_pmkid)
    50 -> context.getString(R.string.softap_disconnect_reason_invalid_mde)
    51 -> context.getString(R.string.softap_disconnect_reason_invalid_fte)
    52 -> context.getString(R.string.softap_disconnect_reason_mesh_peering_cancelled)
    53 -> context.getString(R.string.softap_disconnect_reason_mesh_max_peers)
    54 -> context.getString(R.string.softap_disconnect_reason_mesh_config_policy_violation)
    55 -> context.getString(R.string.softap_disconnect_reason_mesh_close_received)
    56 -> context.getString(R.string.softap_disconnect_reason_mesh_max_retries)
    57 -> context.getString(R.string.softap_disconnect_reason_mesh_confirm_timeout)
    58 -> context.getString(R.string.softap_disconnect_reason_mesh_invalid_gtk)
    59 -> context.getString(R.string.softap_disconnect_reason_mesh_inconsistent_parameters)
    60 -> context.getString(R.string.softap_disconnect_reason_mesh_invalid_security_capability)
    61 -> context.getString(R.string.softap_disconnect_reason_mesh_path_error_no_proxy_info)
    62 -> context.getString(R.string.softap_disconnect_reason_mesh_path_error_no_forwarding_info)
    63 -> context.getString(R.string.softap_disconnect_reason_mesh_path_error_destination_unreachable)
    64 -> context.getString(R.string.softap_disconnect_reason_mac_address_exists_in_mbss)
    65 -> context.getString(R.string.softap_disconnect_reason_mesh_channel_switch_regulatory_required)
    66 -> context.getString(R.string.softap_disconnect_reason_mesh_channel_switch_unspecified)
    else -> fallback
}

fun softApFeatureLabel(
    context: Context,
    feature: Long,
    fallback: String = SoftApCapability.featureLookup(feature, true).replace('_', ' '),
) = when (feature) {
    1L shl 0 -> context.getString(R.string.tethering_manage_wifi_feature_acs_offload)
    1L shl 1 -> context.getString(R.string.tethering_manage_wifi_feature_client_force_disconnect)
    1L shl 2 -> context.getString(R.string.tethering_manage_wifi_feature_wpa3_sae)
    1L shl 3 -> context.getString(R.string.tethering_manage_wifi_feature_mac_address_customization)
    1L shl 4 -> context.getString(R.string.tethering_manage_wifi_feature_ieee80211ax)
    1L shl 5 -> context.getString(
        R.string.tethering_manage_wifi_feature_band_supported,
        softApBandLabel(context, SoftApConfiguration.BAND_2GHZ),
    )
    1L shl 6 -> context.getString(
        R.string.tethering_manage_wifi_feature_band_supported,
        softApBandLabel(context, SoftApConfiguration.BAND_5GHZ),
    )
    1L shl 7 -> context.getString(
        R.string.tethering_manage_wifi_feature_band_supported,
        softApBandLabel(context, SoftApConfiguration.BAND_6GHZ),
    )
    1L shl 8 -> context.getString(
        R.string.tethering_manage_wifi_feature_band_supported,
        softApBandLabel(context, SoftApConfiguration.BAND_60GHZ),
    )
    1L shl 9 -> context.getString(R.string.tethering_manage_wifi_feature_ieee80211be)
    1L shl 10 -> context.getString(R.string.tethering_manage_wifi_feature_wpa3_owe_transition)
    1L shl 11 -> context.getString(R.string.tethering_manage_wifi_feature_wpa3_owe)
    1L shl 12 -> context.getString(R.string.tethering_manage_wifi_feature_mlo)
    else -> fallback
}
