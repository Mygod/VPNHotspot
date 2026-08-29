package android.net;

import androidx.annotation.RequiresApi;

/**
 * Hidden builder used only for the minimal legacy score required by {@link NetworkAgent}.
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkScore.java#216
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkScore.java#245
 */
@RequiresApi(31)
public abstract class NetworkScore {
    public static final class Builder {
        public Builder() {
        }

        public Builder setLegacyInt(int score) {
            throw new UnsupportedOperationException();
        }

        public NetworkScore build() {
            throw new UnsupportedOperationException();
        }
    }
}
