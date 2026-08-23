package android.net;

import androidx.annotation.RequiresApi;

/**
 * Binary-name stub for the nested builder: {@link NetworkCapabilities} itself is in the SDK, so the
 * nested type cannot be declared inside it here. A fresh builder starts from
 * {@code new NetworkCapabilities()}, whose allowed-UID set is empty, so VPNHotspot never needs the
 * blocked {@code setAllowedUids} to submit an empty set.
 *
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkCapabilities.java#374
 */
@RequiresApi(31)
public final class NetworkCapabilities$Builder {
    public NetworkCapabilities$Builder() {
    }

    public NetworkCapabilities$Builder addTransportType(int transportType) {
        throw new UnsupportedOperationException();
    }

    public NetworkCapabilities$Builder addCapability(int capability) {
        throw new UnsupportedOperationException();
    }

    public NetworkCapabilities$Builder removeCapability(int capability) {
        throw new UnsupportedOperationException();
    }

    public NetworkCapabilities$Builder setNetworkSpecifier(NetworkSpecifier specifier) {
        throw new UnsupportedOperationException();
    }

    public NetworkCapabilities build() {
        throw new UnsupportedOperationException();
    }
}
