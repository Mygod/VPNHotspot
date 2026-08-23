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
     * Requires {@code NETWORK_SETTINGS}, which the service reports through the listener as
     * {@code TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION} instead of throwing, so the result code must
     * be consumed. Declared {@code oneway}, so the transact does not wait on the service. Only sets a
     * global preference; it does not force upstream reselection.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringService.java#286
     */
    @RequiresApi(33)
    void setPreferTestNetworks(boolean prefer, IIntResultListener listener) throws RemoteException;
}
