package android.net;

import androidx.annotation.RequiresApi;

/**
 * Binary-name stub for the hidden nested builder. On Android 12+, its fresh allowed-UID set is empty,
 * avoiding blocked {@code setAllowedUids}; Android 11 uses the older restricted-network permission model.
 * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/NetworkCapabilities.java#2140
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkCapabilities.java#2676
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkCapabilities.java#3101
 */
@RequiresApi(30)
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
