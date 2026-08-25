package android.net;

import android.os.ParcelFileDescriptor;

public abstract class TestNetworkInterface {
    public abstract ParcelFileDescriptor getFileDescriptor();

    public abstract String getInterfaceName();
}
