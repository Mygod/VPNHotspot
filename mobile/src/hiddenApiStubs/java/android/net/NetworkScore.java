package android.net;

import androidx.annotation.RequiresApi;

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
