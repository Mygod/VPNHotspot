package android.net;

import android.os.ParcelFileDescriptor;
import androidx.annotation.RequiresApi;

/**
 * Return carrier for the TUN descriptor and interface name created by {@link TestNetworkManager}.
 * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/TestNetworkInterface.java#35
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/TestNetworkInterface.java#58
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/TestNetworkInterface.java#94
 */
@RequiresApi(30)
public abstract class TestNetworkInterface {
    public abstract ParcelFileDescriptor getFileDescriptor();

    public abstract String getInterfaceName();
}
