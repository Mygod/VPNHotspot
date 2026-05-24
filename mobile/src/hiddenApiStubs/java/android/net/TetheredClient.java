package android.net;

import android.os.Parcelable;

import java.util.List;

public abstract class TetheredClient implements Parcelable {
    public abstract List<AddressInfo> getAddresses();

    public abstract MacAddress getMacAddress();

    public abstract int getTetheringType();

    public abstract static class AddressInfo {
        public abstract LinkAddress getAddress();

        public abstract String getHostname();
    }
}
