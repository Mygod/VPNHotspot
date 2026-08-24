package android.net;

import android.os.IInterface;
import android.os.RemoteException;
import androidx.annotation.RequiresApi;

@RequiresApi(30)
public interface ITetheringConnector extends IInterface {
    void stopTethering(int type, String callerPkg, IIntResultListener listener) throws RemoteException;

    /**
     * Expected to be used on API 31+ when present before falling back to the API 30 overload.
     */
    void stopTethering(int type, String callerPkg, String callingAttributionTag, IIntResultListener listener)
            throws RemoteException;
}
