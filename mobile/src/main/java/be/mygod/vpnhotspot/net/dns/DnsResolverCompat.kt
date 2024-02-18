package be.mygod.vpnhotspot.net.dns

import android.annotation.TargetApi
import android.app.ActivityManager
import android.net.DnsResolver
import android.net.Network
import android.os.Build
import android.os.CancellationSignal
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.App.Companion.app
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import org.xbill.DNS.AAAARecord
import org.xbill.DNS.ARecord
import org.xbill.DNS.DClass
import org.xbill.DNS.Flags
import org.xbill.DNS.Message
import org.xbill.DNS.Opcode
import org.xbill.DNS.Section
import org.xbill.DNS.Type
import java.io.IOException
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

sealed class DnsResolverCompat {
    companion object : DnsResolverCompat() {
        private val instance by lazy {
            when (Build.VERSION.SDK_INT) {
                in 29..Int.MAX_VALUE -> DnsResolverCompat29
                in 23 until 29 -> DnsResolverCompat23
                else -> error("Unsupported API level")
            }
        }

        override suspend fun resolve(network: Network, host: String) = instance.resolve(network, host)
        override suspend fun resolveRaw(network: Network, query: ByteArray) = instance.resolveRaw(network, query)

        // additional platform-independent DNS helpers

        /**
         * TTL returned from localResolver is set to 120. Android API does not provide TTL,
         * so we suppose Android apps should not care about TTL either.
         */
        private const val TTL = 120L

        fun prepareDnsResponse(request: Message) = Message(request.header.id).apply {
            header.setFlag(Flags.QR.toInt())    // this is a response
            header.setFlag(Flags.RA.toInt())    // recursion available
            if (request.header.getFlag(Flags.RD.toInt())) header.setFlag(Flags.RD.toInt())
            request.question?.also { addRecord(it, Section.QUESTION) }
        }
    }

    abstract suspend fun resolve(network: Network, host: String): Array<InetAddress>
    abstract suspend fun resolveRaw(network: Network, query: ByteArray): ByteArray

    private data object DnsResolverCompat23 : DnsResolverCompat() {
        /**
         * This dispatcher is used for noncancellable possibly-forever-blocking operations in network IO.
         *
         * See also: https://issuetracker.google.com/issues/133874590
         */
        private val unboundedIO by lazy {
            if (app.getSystemService<ActivityManager>()!!.isLowRamDevice) Dispatchers.IO
            else Executors.newCachedThreadPool().asCoroutineDispatcher()
        }

        override suspend fun resolve(network: Network, host: String) =
            withContext(unboundedIO) { network.getAllByName(host) }

        override suspend fun resolveRaw(network: Network, query: ByteArray): ByteArray {
            val request = Message(query)
            when (val opcode = request.header.opcode) {
                Opcode.QUERY -> { }
                else -> throw UnsupportedOperationException("Unsupported opcode $opcode")
            }
            val question = request.question
            val isIpv6 = when (val type = question?.type) {
                Type.A -> false
                Type.AAAA -> true
                else -> throw UnsupportedOperationException("Unsupported query type $type")
            }
            val host = question.name.canonicalize().toString(true)
            return prepareDnsResponse(request).apply {
                for (address in resolve(network, host).asIterable().run {
                    if (isIpv6) filterIsInstance<Inet6Address>() else filterIsInstance<Inet4Address>()
                }) addRecord(when (address) {
                    is Inet4Address -> ARecord(question.name, DClass.IN, TTL, address)
                    is Inet6Address -> AAAARecord(question.name, DClass.IN, TTL, address)
                    else -> error("Unknown address $address")
                }, Section.ANSWER)
            }.toWire()
        }
    }

    @TargetApi(29)
    private data object DnsResolverCompat29 : DnsResolverCompat(), Executor {
        /**
         * This executor will run on its caller directly. On Q beta 3 thru 4, this results in calling in main thread.
         */
        override fun execute(command: Runnable) = command.run()

        override suspend fun resolve(network: Network, host: String) = suspendCancellableCoroutine { cont ->
            val signal = CancellationSignal()
            cont.invokeOnCancellation { signal.cancel() }
            // retry should be handled by client instead
            DnsResolver.getInstance().query(network, host, DnsResolver.FLAG_NO_RETRY, this, signal,
                object : DnsResolver.Callback<Collection<InetAddress>> {
                    override fun onAnswer(answer: Collection<InetAddress>, rcode: Int) =
                        cont.resume(answer.toTypedArray())
                    override fun onError(error: DnsResolver.DnsException) =
                        cont.resumeWithException(IOException(error))
                })
        }

        override suspend fun resolveRaw(network: Network, query: ByteArray) = suspendCancellableCoroutine { cont ->
            val signal = CancellationSignal()
            cont.invokeOnCancellation { signal.cancel() }
            DnsResolver.getInstance().rawQuery(network, query, DnsResolver.FLAG_NO_RETRY, this, signal,
                object : DnsResolver.Callback<ByteArray> {
                    override fun onAnswer(answer: ByteArray, rcode: Int) = cont.resume(answer)
                    override fun onError(error: DnsResolver.DnsException) =
                        cont.resumeWithException(IOException(error))
                })
        }
    }
}
