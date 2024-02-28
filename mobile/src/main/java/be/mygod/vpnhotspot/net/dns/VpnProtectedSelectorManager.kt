package be.mygod.vpnhotspot.net.dns

import android.net.VpnService
import io.ktor.network.selector.SelectInterest
import io.ktor.network.selector.Selectable
import io.ktor.network.selector.SelectorManager
import timber.log.Timber
import java.net.ProtocolFamily
import java.nio.channels.spi.SelectorProvider

class VpnProtectedSelectorManager(private val manager: SelectorManager) : SelectorProvider(), SelectorManager {
    private val protector = VpnService()
    private fun checkProtect(success: Boolean) {
        if (!success) Timber.w(Exception("protect failed"))
    }

    override fun openDatagramChannel() = manager.provider.openDatagramChannel().apply {
        checkProtect(protector.protect(socket()))
    }
    override fun openDatagramChannel(family: ProtocolFamily?) = manager.provider.openDatagramChannel(family).apply {
        checkProtect(protector.protect(socket()))
    }
    override fun openPipe() = manager.provider.openPipe()
    override fun openSelector() = manager.provider.openSelector()
    override fun openServerSocketChannel() = manager.provider.openServerSocketChannel()
    override fun openSocketChannel() = manager.provider.openSocketChannel().apply {
        checkProtect(protector.protect(socket()))
    }

    override val coroutineContext get() = manager.coroutineContext
    override val provider get() = this
    override fun close() = manager.close()
    override fun notifyClosed(selectable: Selectable) = manager.notifyClosed(selectable)
    override suspend fun select(selectable: Selectable, interest: SelectInterest) = manager.select(selectable, interest)
}
