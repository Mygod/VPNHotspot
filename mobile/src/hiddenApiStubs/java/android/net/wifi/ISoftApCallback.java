package android.net.wifi;

import android.os.Binder;
import android.os.IBinder;
import android.os.IInterface;

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
