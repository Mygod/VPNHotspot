package android.net;

import android.os.Parcelable;
import android.os.ParcelFileDescriptor;

public abstract class TestNetworkInterface implements Parcelable {
    public abstract ParcelFileDescriptor getFileDescriptor();

    public abstract String getInterfaceName();
}
