package android.net;

import android.os.Parcelable;
import androidx.annotation.RequiresApi;

@RequiresApi(31)
public abstract class NetworkScore implements Parcelable {
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
