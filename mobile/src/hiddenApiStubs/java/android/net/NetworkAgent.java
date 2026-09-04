package android.net;

import android.content.Context;
import android.os.Looper;
import androidx.annotation.RequiresApi;

/**
 * App-hosted agent. Android 11 has the integer-score constructor and unwanted callback; Android 12 adds
 * the created/destroyed callbacks.
 * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/NetworkAgent.java#375
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#590
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#675
 */
public abstract class NetworkAgent {
    @RequiresApi(30)
    public NetworkAgent(Context context, Looper looper, String logTag, NetworkCapabilities nc,
                        LinkProperties lp, int score, NetworkAgentConfig config,
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
