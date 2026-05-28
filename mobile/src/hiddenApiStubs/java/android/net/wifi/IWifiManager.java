package android.net.wifi;

import android.os.IInterface;
import android.os.RemoteException;

public interface IWifiManager extends IInterface {
    void registerSoftApCallback(ISoftApCallback callback) throws RemoteException;

    void unregisterSoftApCallback(ISoftApCallback callback) throws RemoteException;
}
