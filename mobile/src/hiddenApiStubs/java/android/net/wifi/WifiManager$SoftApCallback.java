package android.net.wifi;

import androidx.annotation.RequiresApi;

import java.util.List;

public interface WifiManager$SoftApCallback {
    @RequiresApi(30)
    default void onBlockedClientConnecting(WifiClient client, int blockedReason) {
    }

    @RequiresApi(30)
    default void onCapabilityChanged(SoftApCapability capability) {
    }

    @RequiresApi(30)
    default void onClientsDisconnected(SoftApInfo info, List<WifiClient> clients) {
    }

    @RequiresApi(30)
    default void onConnectedClientsChanged(List<WifiClient> clients) {
    }

    /**
     * Used on API 30 only as the single-instance Soft AP info callback.
     */
    @RequiresApi(30)
    default void onInfoChanged(SoftApInfo info) {
    }

    /**
     * Expected to be used on API 31+ for bridged Soft AP info callbacks.
     */
    @RequiresApi(30)
    default void onInfoChanged(List<SoftApInfo> infos) {
    }

    /**
     * Used only on API 29 for legacy Soft AP client counts.
     */
    default void onNumClientsChanged(int numClients) {
    }

    default void onStateChanged(int state, int failureReason) {
    }
}
