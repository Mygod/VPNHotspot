package be.mygod.vpnhotspot.root.daemon

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test
import java.io.IOException

class DaemonAbiTest {
    @Test
    fun daemonAbiReadsApkEntryPath() {
        assertEquals("arm64-v8a", DaemonAbi.daemonAbi("/data/app/base.apk!/lib/arm64-v8a/libvpnhotspotd.so"))
    }

    @Test
    fun daemonAbiReadsExtractedLibraryPath() {
        assertEquals("arm64-v8a", DaemonAbi.daemonAbi("/data/app/example/lib/arm64/libvpnhotspotd.so"))
    }

    @Test
    fun daemonAbiReturnsNullForUnexpectedPath() {
        assertNull(DaemonAbi.daemonAbi("/data/app/base.apk!/assets/libvpnhotspotd.so"))
    }

    @Test
    fun rootAbiUsesKernelMachineAndProcessBitness() {
        assertEquals("x86_64", DaemonAbi.rootAbi("x86_64", true))
        assertEquals("x86", DaemonAbi.rootAbi("x86_64", false))
        assertEquals("arm64-v8a", DaemonAbi.rootAbi("aarch64", true))
        assertEquals("armeabi-v7a", DaemonAbi.rootAbi("aarch64", false))
        assertEquals("x86", DaemonAbi.rootAbi("i686", false))
        assertEquals("armeabi-v7a", DaemonAbi.rootAbi("armv8l", false))
    }

    @Test
    fun checkRejectsMismatchedDaemonAbi() {
        val e = assertThrows(IOException::class.java) {
            DaemonAbi.check("/data/app/base.apk!/lib/arm64-v8a/libvpnhotspotd.so", "x86_64", true)
        }
        assertEquals("Wrong APK variant installed: this APK contains the arm64-v8a vpnhotspotd, " +
                "but this Android system runs x86_64 root binaries. Install the x86_64 APK and try again.",
            e.message)
    }

    @Test
    fun checkAcceptsMatchingDaemonAbi() {
        DaemonAbi.check("/data/app/base.apk!/lib/x86_64/libvpnhotspotd.so", "x86_64", true)
    }
}
