package android.net.wifi;

import android.os.Bundle;
import android.os.IBinder;
import android.os.IInterface;
import android.os.RemoteException;
import androidx.annotation.RequiresApi;

public interface IWifiManager extends IInterface {
    @RequiresApi(33)
    void registerLocalOnlyHotspotSoftApCallback(ISoftApCallback callback, Bundle extras) throws RemoteException;

    /**
     * Legacy Soft AP callback registration ABI used by AOSP API 29 and API 30.
     */
    void registerSoftApCallback(IBinder binder, ISoftApCallback callback, int callbackIdentifier)
            throws RemoteException;

    @RequiresApi(31)
    void registerSoftApCallback(ISoftApCallback callback) throws RemoteException;

    @RequiresApi(33)
    void unregisterLocalOnlyHotspotSoftApCallback(ISoftApCallback callback, Bundle extras) throws RemoteException;

    /**
     * Legacy Soft AP callback unregistration ABI used by AOSP API 29 and API 30.
     */
    void unregisterSoftApCallback(int callbackIdentifier) throws RemoteException;

    @RequiresApi(31)
    void unregisterSoftApCallback(ISoftApCallback callback) throws RemoteException;
}
