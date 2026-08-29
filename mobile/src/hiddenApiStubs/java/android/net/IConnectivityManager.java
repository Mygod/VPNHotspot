package android.net;

import android.os.IBinder;
import android.os.IInterface;
import android.os.RemoteException;

/**
 * Unsupported-app-usage reachability keeps this interface unrelocated; {@code ITestNetworkManager} is
 * relocated and therefore reflected by runtime name.
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/Android.bp#359
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/apex/hiddenapi/hiddenapi-unsupported.txt
 */
public interface IConnectivityManager extends IInterface {
    /**
     * Returns the TestNetwork service binder without depending on its relocated interface name.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/IConnectivityManager.aidl#208
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/IConnectivityManager.aidl#215
     */
    IBinder startOrGetTestNetworkService() throws RemoteException;

    /**
     * Direct release preserves the exact request handle for retry; the public callback wrapper discards it.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/IConnectivityManager.aidl#169
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/IConnectivityManager.aidl#174
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#5578
     */
    void releaseNetworkRequest(NetworkRequest request) throws RemoteException;

    abstract class Stub {
        public static IConnectivityManager asInterface(IBinder binder) {
            throw new UnsupportedOperationException();
        }
    }
}
