package android.net.wifi;

import android.os.Parcelable;

public abstract class SoftApCapability implements Parcelable {
    public static final long SOFTAP_FEATURE_BAND_24G_SUPPORTED = 32L;
    public static final long SOFTAP_FEATURE_BAND_5G_SUPPORTED = 64L;
    public static final long SOFTAP_FEATURE_BAND_6G_SUPPORTED = 128L;
    public static final long SOFTAP_FEATURE_BAND_60G_SUPPORTED = 256L;

    public abstract boolean areFeaturesSupported(long features);

    public abstract String getCountryCode();

    public abstract int getMaxSupportedClients();

    public abstract int[] getSupportedChannelList(int band);
}
