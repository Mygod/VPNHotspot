package be.mygod.vpnhotspot

import java.io.File

class ArpCache(downstream: String? = null) : HashMap<String, String>() {
    companion object {
        private val spaces = " +".toPattern()
        private val mac = "^([0-9a-f]{2}:){5}[0-9a-f]{2}$".toPattern()
    }

    init {
        File("/proc/net/arp").bufferedReader().useLines {
            for (line in it) {
                val parts = line.split(spaces)
                // IP address       HW type     Flags       HW address            Mask     Device
                if (parts.size >= 4 && (downstream == null || parts.getOrNull(5) == downstream) &&
                        mac.matcher(parts[3]).matches()) put(parts[3], parts[0])
            }
        }
    }
}
