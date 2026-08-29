package android.net;

import android.content.Context;
import android.os.Looper;
import androidx.annotation.RequiresApi;

/**
 * API 33+ app-hosted agent. {@link #onNetworkUnwanted()} is always the withdrawal barrier, while destruction
 * is conditional on creation.
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#590
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#675
 */
public abstract class NetworkAgent {
    @RequiresApi(31)
    public NetworkAgent(Context context, Looper looper, String logTag, NetworkCapabilities nc,
                        LinkProperties lp, NetworkScore score, NetworkAgentConfig config,
                        NetworkProvider provider) {
    }

    @RequiresApi(30)
    public Network register() {
        throw new UnsupportedOperationException();
    }

    @RequiresApi(30)
    public void markConnected() {
    }

    @RequiresApi(30)
    public Network getNetwork() {
        throw new UnsupportedOperationException();
    }

    @RequiresApi(30)
    public void unregister() {
    }

    @RequiresApi(31)
    public void onNetworkCreated() {
    }

    @RequiresApi(31)
    public void onNetworkDestroyed() {
    }

    @RequiresApi(30)
    public void onNetworkUnwanted() {
    }
}
