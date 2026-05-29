package android.net.wifi;

import android.net.MacAddress;
import android.os.Parcelable;
import androidx.annotation.RequiresApi;

@RequiresApi(30)
public abstract class WifiClient implements Parcelable {
    @RequiresApi(31)
    public abstract String getApInstanceIdentifier();

    public abstract int getDisconnectReason();

    public abstract MacAddress getMacAddress();
}
