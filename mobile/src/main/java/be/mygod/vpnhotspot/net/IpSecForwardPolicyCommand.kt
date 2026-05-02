package be.mygod.vpnhotspot.net

import android.system.OsConstants
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.vpnhotspot.root.fixPath
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import java.net.Inet4Address
import java.net.InetAddress
import java.net.UnknownHostException

/**
 * Updates the live IPv4 FWD policy in place and intentionally leaves eventual teardown to the
 * platform tunnel lifecycle.
 */
@Parcelize
@RequiresApi(31)
data class IpSecForwardPolicyCommand(private val upstream: String) : RootCommand<ParcelableBoolean> {
    override suspend fun execute(): ParcelableBoolean {
        val dump = withContext(Dispatchers.IO) {
            // Existing tunnel/transform state is only exposed via IIpSecService.dump():
            // https://android.googlesource.com/platform/frameworks/base/+/android-9.0.0_r1/core/java/android/net/IIpSecService.aidl#33
            // https://android.googlesource.com/platform/frameworks/base/+/android-9.0.0_r1/services/core/java/com/android/server/IpSecService.java#1731
            val process = ProcessBuilder("dumpsys", "ipsec").fixPath(true).start()
            process.inputStream.bufferedReader().use { it.readText() }.also {
                check(process.waitFor() == 0) { "dumpsys ipsec failed" }
            }
        }
        val (tunnel, inbound) = findTarget(upstream, dump) ?: return ParcelableBoolean(false)
        Netd.ipSecUpdateSecurityPolicy(
            Netd.service,
            tunnel.groupValues[2].toInt(),
            OsConstants.AF_INET,
            DIRECTION_FWD,
            inbound.groupValues[1],
            inbound.groupValues[2],
            INVALID_SECURITY_PARAMETER_INDEX,
            tunnel.groupValues[4].toInt(),
            FULL_MASK,
            tunnel.groupValues[1].toInt(),
        )
        return ParcelableBoolean(true)
    }

    companion object {
        private val tunnelRecord = Regex(
            """\{mResource=\{super=\{mResourceId=(\d+),\s*pid=\d+,\s*uid=(\d+)\},\s*mInterfaceName=([^,]+),.*?mLocalAddress=[^,]+,\s*mRemoteAddress=[^,]+,\s*mIkey=(\d+),\s*mOkey=\d+\}""",
            setOf(RegexOption.DOT_MATCHES_ALL),
        )
        private val transformRecord = Regex(
            """mConfig=\{mMode=TUNNEL,\s*mSourceAddress=([^,]+),\s*mDestinationAddress=([^,]+),\s*mNetwork=([^,]+),.*?mXfrmInterfaceId=(\d+)\}""",
            setOf(RegexOption.DOT_MATCHES_ALL),
        )

        fun findTarget(upstream: String, dump: String): Pair<MatchResult, MatchResult>? {
            val tunnel = tunnelRecord.findAll(dump).firstOrNull { it.groupValues[3] == upstream } ?: return null
            val ifId = tunnel.groupValues[1].toIntOrNull() ?: return null
            // Inbound tunnel transforms keep mNetwork=null; reuse their outer addresses for the FWD policy.
            val inbound = transformRecord.findAll(dump).firstOrNull { match ->
                match.groupValues[4].toIntOrNull() == ifId &&
                        match.groupValues[3] == "null" &&
                        isIpv4(match.groupValues[1]) && isIpv4(match.groupValues[2])
            } ?: return null
            return tunnel to inbound
        }

        private fun isIpv4(address: String) = try {
            InetAddress.getByName(address) is Inet4Address
        } catch (_: UnknownHostException) {
            false
        }

        /**
         * https://android.googlesource.com/platform/frameworks/base/+/android-12.0.0_r1/core/java/android/net/IpSecManager.java#98
         */
        private const val INVALID_SECURITY_PARAMETER_INDEX = 0
        /**
         * https://android.googlesource.com/platform/frameworks/base/+/android-12.0.0_r1/core/java/android/net/IpSecManager.java#89
         */
        private const val DIRECTION_FWD = 2
        /**
         * https://android.googlesource.com/platform/frameworks/base/+/android-9.0.0_r1/services/core/java/com/android/server/IpSecService.java#1296
         * https://android.googlesource.com/platform/frameworks/base/+/android-9.0.0_r1/services/core/java/com/android/server/IpSecService.java#1715
         */
        private const val FULL_MASK = -1
    }
}
