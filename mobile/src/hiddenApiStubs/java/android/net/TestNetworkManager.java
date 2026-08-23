package android.net;

/**
 * Instances are constructed reflectively because the single constructor's parameter type is the
 * jarjar-relocated {@code ITestNetworkManager}, whose name differs between releases. Only
 * {@link #createTunInterface} is called directly; VPNHotspot never calls {@code setupTestNetwork},
 * which would publish an unrestricted network.
 */
public abstract class TestNetworkManager {
    public abstract TestNetworkInterface createTunInterface(LinkAddress[] linkAddrs);
}
