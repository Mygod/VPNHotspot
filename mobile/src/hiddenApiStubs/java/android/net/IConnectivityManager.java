package android.net;

import android.os.IBinder;
import android.os.IInterface;
import android.os.RemoteException;

/**
 * Naming this type is safe even though the Connectivity module jarjars {@code android.net} by
 * default from API 34 on: the rule generator excludes everything reachable from the module's
 * UnsupportedAppUsage inventory, passed as {@code --unsupportedapi}, and
 * {@code Landroid/net/IConnectivityManager$Stub$Proxy;} members are listed there on every supported
 * release. {@code ITestNetworkManager} is not in that inventory and is therefore relocated, so its
 * name is derived at runtime instead of declared here.
 *
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/Android.bp#359
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/apex/hiddenapi/hiddenapi-unsupported.txt
 */
public interface IConnectivityManager extends IInterface {
    IBinder startOrGetTestNetworkService() throws RemoteException;

    /**
     * Declared so that a session can release the exact {@link NetworkRequest} the service returned, rather
     * than going through {@code ConnectivityManager.unregisterNetworkCallback}. That wrapper removes its
     * process-static mapping and marks the callback already-unregistered on any normal RPC return, including
     * the return of a release that the service authorized against a different UID and therefore ignored - so
     * it can destroy the only local handle a retry has while releasing nothing.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#5578
     */
    void releaseNetworkRequest(NetworkRequest request) throws RemoteException;

    abstract class Stub {
        public static IConnectivityManager asInterface(IBinder binder) {
            throw new UnsupportedOperationException();
        }
    }
}
