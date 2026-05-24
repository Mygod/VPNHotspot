package android.net.wifi;

import java.util.List;

public interface WifiManager$SoftApCallback {
    default void onBlockedClientConnecting(WifiClient client, int blockedReason) {
    }

    default void onCapabilityChanged(SoftApCapability capability) {
    }

    default void onClientsDisconnected(SoftApInfo info, List<WifiClient> clients) {
    }

    default void onConnectedClientsChanged(List<WifiClient> clients) {
    }

    default void onInfoChanged(SoftApInfo info) {
    }

    default void onInfoChanged(List<SoftApInfo> infos) {
    }

    default void onNumClientsChanged(int numClients) {
    }

    default void onStateChanged(int state, int failureReason) {
    }
}
