package be.mygod.vpnhotspot.root

import android.annotation.SuppressLint
import android.os.Parcelable
import android.util.Log
import be.mygod.librootkotlinx.Logger
import be.mygod.librootkotlinx.ParcelableThrowable
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.librootkotlinx.RootFlow
import be.mygod.librootkotlinx.RootServer
import be.mygod.librootkotlinx.RootSession
import be.mygod.librootkotlinx.systemContext
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.consumeEach
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber

object RootManager : RootSession(), Logger {
    private var logThrowable = { priority: Int, t: Throwable ->
        if (priority >= Log.WARN) t.printStackTrace(System.err)
    }
    @Parcelize
    class RootInit : RootCommandNoResult {
        override suspend fun execute() = null.also {
            Timber.plant(object : Timber.DebugTree() {
                @SuppressLint("LogNotTimber")
                override fun log(priority: Int, tag: String?, message: String, t: Throwable?) {
                    if (priority >= Log.WARN) {
                        System.err.println("$priority/$tag: $message")
                        t?.printStackTrace()
                    }
                    if (t == null) {
                        Log.println(priority, tag, message)
                    } else {
                        Log.println(priority, tag, message)
                        Log.d(tag, message, t)
                        logThrowable(priority, t)
                    }
                }
            })
            Logger.me = RootManager
            Services.init { systemContext }
            UnblockCentral.needInit = false
        }
    }
    @Parcelize
    data class LoggedThrowable(val priority: Int, val t: ParcelableThrowable) : Parcelable
    @Parcelize
    class RootInitLogThrowable : RootFlow<LoggedThrowable> {
        override fun flow() = callbackFlow {
            val channel = Channel<Pair<Int, Throwable>>(Channel.CONFLATED) { (priority, t) ->
                if (priority >= Log.WARN) t.printStackTrace(System.err)
            }
            val job = launch {
                channel.consumeEach { (priority, t) -> send(LoggedThrowable(priority, ParcelableThrowable(t))) }
            }
            logThrowable = { priority, t ->
                channel.trySend(priority to t).exceptionOrNull()?.printStackTrace(System.err)
            }
            awaitClose {
                logThrowable = { priority, t ->
                    if (priority >= Log.WARN) t.printStackTrace(System.err)
                }
                channel.close()
                job.cancel()
            }
        }.buffer(Channel.UNLIMITED)
    }

    override fun d(m: String?, t: Throwable?) = Timber.d(t, m)
    override fun e(m: String?, t: Throwable?) = Timber.e(t, m)
    override fun i(m: String?, t: Throwable?) = Timber.i(t, m)
    override fun w(m: String?, t: Throwable?) = Timber.w(t, m)

    override suspend fun initServer(server: RootServer) {
        Logger.me = this
        server.init(app)
        server.execute(RootInit())
        GlobalScope.launch {
            try {
                server.flow(RootInitLogThrowable()).collect { (priority, p) ->
                    Timber.tag("RootRemote").log(priority, p.unwrap())
                }
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }
}
