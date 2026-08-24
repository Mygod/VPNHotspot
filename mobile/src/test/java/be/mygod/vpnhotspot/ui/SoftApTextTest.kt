package be.mygod.vpnhotspot.ui

import android.net.wifi.ScanResult
import be.mygod.vpnhotspot.R
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class SoftApTextTest {
    @Test
    fun wifiP2pConnectionInfoLabel_formatsMatchingNss() {
        assertEquals("Wi\u2011Fi 6, 80 MHz, 2\u00d72", label(
            ScanResult.WIFI_STANDARD_11AX,
            ScanResult.CHANNEL_WIDTH_80MHZ,
            2,
            2,
        ))
    }

    @Test
    fun wifiP2pConnectionInfoLabel_formatsAsymmetricNss() {
        assertEquals("Wi\u2011Fi 6, 80 MHz, Tx2/Rx1", label(
            ScanResult.WIFI_STANDARD_11AX,
            ScanResult.CHANNEL_WIDTH_80MHZ,
            2,
            1,
        ))
    }

    @Test
    fun wifiP2pConnectionInfoLabel_formatsPartialOutput() {
        assertEquals("Wi\u2011Fi 7, 320 MHz", label(
            ScanResult.WIFI_STANDARD_11BE,
            ScanResult.CHANNEL_WIDTH_320MHZ,
            0,
            0,
        ))
        assertEquals("160 MHz (80 + 80 MHz), Tx2", label(
            ScanResult.WIFI_STANDARD_UNKNOWN,
            ScanResult.CHANNEL_WIDTH_80MHZ_PLUS_MHZ,
            2,
            0,
        ))
    }

    @Test
    fun wifiP2pConnectionInfoLabel_omitsInvalidFields() {
        assertEquals("Rx1", label(-1, -1, -1, 1))
    }

    @Test
    fun wifiP2pConnectionInfoLabel_returnsNullWhenNothingDisplayable() {
        assertNull(label(ScanResult.WIFI_STANDARD_UNKNOWN, -1, 0, 0))
    }

    private fun label(wifiStandard: Int, channelWidth: Int, txNss: Int, rxNss: Int) =
        wifiP2pConnectionInfoLabel(wifiStandard, channelWidth, txNss, rxNss, ::string)

    private fun string(id: Int, args: List<Int>) = when (id) {
        R.string.wifi_channel_width_mhz -> "${args[0]} MHz"
        R.string.wifi_channel_width_160mhz_80_plus_80 -> "160 MHz (80 + 80 MHz)"
        R.string.wifi_standard_generation -> "Wi\u2011Fi ${args[0]}"
        R.string.wifi_standard_legacy -> "Legacy Wi\u2011Fi"
        R.string.wifi_standard_wigig -> "WiGig"
        R.string.wifi_p2p_connection_info_separator -> ", "
        R.string.wifi_p2p_connection_info_nss -> "${args[0]}\u00d7${args[1]}"
        R.string.wifi_p2p_connection_info_tx_nss -> "Tx${args[0]}"
        R.string.wifi_p2p_connection_info_rx_nss -> "Rx${args[0]}"
        R.string.wifi_p2p_connection_info_tx_rx_nss -> "Tx${args[0]}/Rx${args[1]}"
        else -> error("Unexpected string resource $id")
    }
}
