package android.net;

import android.os.IInterface;
import android.os.RemoteException;

public interface ITetheringConnector extends IInterface {
    void stopTethering(int type, String callerPkg, IIntResultListener listener) throws RemoteException;

    void stopTethering(int type, String callerPkg, String callingAttributionTag, IIntResultListener listener)
            throws RemoteException;
}
