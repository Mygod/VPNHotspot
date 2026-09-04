package android.net;

import androidx.annotation.RequiresApi;

/**
 * Only referenced as the declared type of the trailing {@link NetworkAgent} constructor parameter,
 * which VPNHotspot always passes as null.
 * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/NetworkProvider.java#44
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkProvider.java#50
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkProvider.java#50
 */
@RequiresApi(30)
public abstract class NetworkProvider {
}
