package android.net.wifi;

import android.annotation.TargetApi;
import android.net.MacAddress;
import android.os.Parcelable;

import java.util.List;

public abstract class SoftApInfo implements Parcelable {
    @TargetApi(33)
    public static final int CHANNEL_WIDTH_AUTO = -1;

    public static final int CHANNEL_WIDTH_INVALID = 0;

    public abstract long getAutoShutdownTimeoutMillis();

    public abstract int getBandwidth();

    public abstract MacAddress getBssid();

    public abstract int getFrequency();

    public abstract MacAddress getMldAddress();

    public abstract String getApInstanceIdentifier();

    public abstract List<OuiKeyedData> getVendorData();

    public abstract int getWifiStandard();
}
