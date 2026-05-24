package android.net;

import android.os.Binder;
import android.os.IBinder;
import android.os.IInterface;
import android.os.RemoteException;

public interface IIntResultListener extends IInterface {
    void onResult(int resultCode) throws RemoteException;

    abstract class Stub extends Binder implements IIntResultListener {
        public Stub() {
            attachInterface(this, "android.net.IIntResultListener");
        }

        @Override
        public IBinder asBinder() {
            return this;
        }
    }
}
