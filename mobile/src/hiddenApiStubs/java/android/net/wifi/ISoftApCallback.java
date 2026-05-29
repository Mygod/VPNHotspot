package android.net.wifi;

import android.os.Binder;
import android.os.IBinder;
import android.os.IInterface;
import androidx.annotation.RequiresApi;

/**
 * Used on API 31+ for direct IWifiManager Soft AP callback binder registration.
 */
@RequiresApi(30)
public interface ISoftApCallback extends IInterface {
    abstract class Stub extends Binder implements ISoftApCallback {
        public Stub() {
            attachInterface(this, "android.net.wifi.ISoftApCallback");
        }

        public static ISoftApCallback asInterface(IBinder obj) {
            throw new UnsupportedOperationException();
        }

        @Override
        public IBinder asBinder() {
            return this;
        }
    }
}
