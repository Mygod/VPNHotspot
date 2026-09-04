package android.net;

import androidx.annotation.RequiresApi;

/**
 * Hidden builder used only to supply legacy TEST metadata to {@link NetworkAgent}.
 * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/NetworkAgentConfig.java#240
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgentConfig.java#301
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgentConfig.java#302
 */
@RequiresApi(30)
public abstract class NetworkAgentConfig {
    public static final class Builder {
        public Builder() {
        }

        public Builder setLegacyType(int legacyType) {
            throw new UnsupportedOperationException();
        }

        public Builder setLegacyTypeName(String legacyTypeName) {
            throw new UnsupportedOperationException();
        }

        public NetworkAgentConfig build() {
            throw new UnsupportedOperationException();
        }
    }
}
