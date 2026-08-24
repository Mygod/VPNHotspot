package be.mygod.vpnhotspot.root

import android.os.Parcelable
import android.system.OsConstants
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.RootCommandNoResult
import kotlinx.parcelize.Parcelize

/**
 * Updates the live IPv4 FWD policy in place and intentionally leaves eventual teardown to the
 * platform tunnel lifecycle.
 */
@Parcelize
@RequiresApi(31)
data class IpSecForwardPolicyCommand(
    private val uid: Int,
    private val sourceAddress: String,
    private val destinationAddress: String,
    private val markValue: Int,
    private val xfrmInterfaceId: Int,
) : RootCommandNoResult {
    override suspend fun execute(): Parcelable? {
        Netd.ipSecUpdateSecurityPolicy(
            Netd.service,
            uid,
            OsConstants.AF_INET,
            DIRECTION_FWD,
            sourceAddress,
            destinationAddress,
            INVALID_SECURITY_PARAMETER_INDEX,
            markValue,
            FULL_MASK,
            xfrmInterfaceId,
        )
        return null
    }

    companion object {
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
