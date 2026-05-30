package android.net.wifi;

import android.os.Bundle;
import android.os.IInterface;
import android.os.RemoteException;
import androidx.annotation.RequiresApi;

@RequiresApi(31)
public interface IWifiManager extends IInterface {
    @RequiresApi(33)
    void registerLocalOnlyHotspotSoftApCallback(ISoftApCallback callback, Bundle extras) throws RemoteException;

    void registerSoftApCallback(ISoftApCallback callback) throws RemoteException;

    @RequiresApi(33)
    void unregisterLocalOnlyHotspotSoftApCallback(ISoftApCallback callback, Bundle extras) throws RemoteException;

    void unregisterSoftApCallback(ISoftApCallback callback) throws RemoteException;
}
