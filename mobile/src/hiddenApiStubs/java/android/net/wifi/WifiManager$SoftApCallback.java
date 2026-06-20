package android.net.wifi;

import androidx.annotation.RequiresApi;

import java.util.List;

@RequiresApi(30)
public interface WifiManager$SoftApCallback {
    default void onBlockedClientConnecting(WifiClient client, int blockedReason) {
    }

    default void onCapabilityChanged(SoftApCapability capability) {
    }

    default void onClientsDisconnected(SoftApInfo info, List<WifiClient> clients) {
    }

    default void onConnectedClientsChanged(List<WifiClient> clients) {
    }

    /**
     * Used on API 30 only as the single-instance Soft AP info callback.
     */
    default void onInfoChanged(SoftApInfo info) {
    }

    /**
     * Expected to be used on API 31+ for bridged Soft AP info callbacks.
     */
    default void onInfoChanged(List<SoftApInfo> infos) {
    }

    default void onStateChanged(int state, int failureReason) {
    }
}
