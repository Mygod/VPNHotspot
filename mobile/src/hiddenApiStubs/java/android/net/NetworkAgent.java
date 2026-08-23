package android.net;

import android.content.Context;
import android.os.Looper;
import androidx.annotation.RequiresApi;

/**
 * Shizuku mode publishes its restricted TestNetwork through an app-hosted agent, so this class is
 * extended rather than only called. VPNHotspot uses it on API 33+ only; the unrelated API 29 shape
 * of this class is not declared here.
 *
 * <p>{@link #register()} looks the privileged manager up through the constructor's context by
 * {@code CONNECTIVITY_SERVICE} name, which is why the agent must be built with the private context.
 *
 * <p>{@link #getNetwork()} is what a registration that threw is classified by: the platform assigns it
 * before {@link #register()} returns, so it is the only answer available once the return value is gone.
 * {@link #onNetworkUnwanted()} is the terminal agent-channel callback a withdrawal requires: it is delivered
 * from {@code NetworkAgentInfo.disconnect()} whether or not a native network was ever created, which is why
 * it - rather than {@link #onNetworkDestroyed()} alone - is the barrier that proves a known agent is gone.
 * Both are {@code sdk,test-api} on every supported release.
 *
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java
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
