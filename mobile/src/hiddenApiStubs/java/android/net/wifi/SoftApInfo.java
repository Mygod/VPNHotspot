package android.net.wifi;

import android.net.MacAddress;
import android.os.Parcelable;
import androidx.annotation.RequiresApi;

import java.util.List;

@RequiresApi(30)
public abstract class SoftApInfo implements Parcelable {
    @RequiresApi(33)
    public static final int CHANNEL_WIDTH_AUTO = -1;

    /**
     * Currently only used on API 30.
     */
    public static final int CHANNEL_WIDTH_INVALID = 0;

    @RequiresApi(31)
    public abstract long getAutoShutdownTimeoutMillis();

    public abstract int getBandwidth();

    @RequiresApi(31)
    public abstract MacAddress getBssid();

    public abstract int getFrequency();

    /**
     * Used on API 30+ when present; missing implementations are tolerated below API 36.
     */
    public abstract MacAddress getMldAddress();

    @RequiresApi(31)
    public abstract String getApInstanceIdentifier();

    @RequiresApi(35)
    public abstract List<OuiKeyedData> getVendorData();

    @RequiresApi(31)
    public abstract int getWifiStandard();
}
