package android.net;

import androidx.annotation.RequiresApi;

/**
 * Binary-name stub for the hidden nested builder. Its fresh allowed-UID set is empty, avoiding blocked
 * {@code setAllowedUids}.
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkCapabilities.java#2676
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkCapabilities.java#3101
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
