package android.net.wifi;

import android.net.MacAddress;
import android.os.Parcelable;

public abstract class WifiClient implements Parcelable {
    public abstract String getApInstanceIdentifier();

    public abstract int getDisconnectReason();

    public abstract MacAddress getMacAddress();
}
