package android.net;

/**
 * Instances are constructed reflectively because the single constructor's parameter type is the
 * jarjar-relocated {@code ITestNetworkManager}, whose name differs between releases. Only
 * {@link #createTunInterface} is called directly; VPNHotspot never calls {@code setupTestNetwork},
 * which would publish an unrestricted network.
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/TestNetworkManager.java#151
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/TestNetworkManager.java#156
 */
public abstract class TestNetworkManager {
    public abstract TestNetworkInterface createTunInterface(LinkAddress[] linkAddrs);
}
