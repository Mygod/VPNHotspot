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

    /**
     * The oneway call reports permission denial through the listener and does not force reselection.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringService.java#208
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringService.java#286
     */
    @RequiresApi(33)
    void setPreferTestNetworks(boolean prefer, IIntResultListener listener) throws RemoteException;
}
