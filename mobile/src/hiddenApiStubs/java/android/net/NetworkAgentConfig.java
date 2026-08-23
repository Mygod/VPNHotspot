package android.net;

import android.os.Parcelable;
import androidx.annotation.RequiresApi;

@RequiresApi(30)
public abstract class NetworkAgentConfig implements Parcelable {
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
