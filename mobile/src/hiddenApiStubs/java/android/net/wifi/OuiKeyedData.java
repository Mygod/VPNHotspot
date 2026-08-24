package android.net.wifi;

import android.os.Parcelable;
import android.os.PersistableBundle;
import androidx.annotation.RequiresApi;

@RequiresApi(35)
public abstract class OuiKeyedData implements Parcelable {
    public abstract PersistableBundle getData();

    public abstract int getOui();

    public static final class Builder {
        public Builder(int oui, PersistableBundle data) {
        }

        public OuiKeyedData build() {
            throw new UnsupportedOperationException();
        }
    }
}
