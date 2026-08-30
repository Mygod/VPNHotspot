package android.net;

import androidx.annotation.RequiresApi;

/**
 * Instances are constructed reflectively through the compatible interface constructor because its
 * {@code ITestNetworkManager} parameter can be jarjar-relocated and other constructors may be added. Only
 * {@link #createTunInterface} is called directly; VPNHotspot never calls {@code setupTestNetwork},
 * which would publish an unrestricted network.
 * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/TestNetworkManager.java#49
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/TestNetworkManager.java#151
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/TestNetworkManager.java#156
 */
@RequiresApi(30)
public abstract class TestNetworkManager {
    public abstract TestNetworkInterface createTunInterface(LinkAddress[] linkAddrs);
}
