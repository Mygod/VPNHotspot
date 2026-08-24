package android.net.wifi;

import android.os.Parcelable;
import androidx.annotation.RequiresApi;

@RequiresApi(30)
public abstract class SoftApCapability implements Parcelable {
    @RequiresApi(31)
    public static final long SOFTAP_FEATURE_BAND_24G_SUPPORTED = 32L;
    @RequiresApi(31)
    public static final long SOFTAP_FEATURE_BAND_5G_SUPPORTED = 64L;
    @RequiresApi(31)
    public static final long SOFTAP_FEATURE_BAND_6G_SUPPORTED = 128L;
    @RequiresApi(31)
    public static final long SOFTAP_FEATURE_BAND_60G_SUPPORTED = 256L;

    public abstract boolean areFeaturesSupported(long features);

    @RequiresApi(31)
    public abstract String getCountryCode();

    public abstract int getMaxSupportedClients();

    @RequiresApi(31)
    public abstract int[] getSupportedChannelList(int band);
}
