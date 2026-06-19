package be.mygod.vpnhotspot.root

import android.annotation.SuppressLint
import android.os.Parcelable
import android.util.Log
import be.mygod.librootkotlinx.Logger
import be.mygod.librootkotlinx.ParcelableThrowable
import be.mygod.librootkotlinx.RootFlow
import be.mygod.librootkotlinx.RootProcess
import be.mygod.librootkotlinx.RootServer
import be.mygod.librootkotlinx.RootSession
import be.mygod.librootkotlinx.io.awaitExit
import be.mygod.librootkotlinx.io.pid
import be.mygod.librootkotlinx.systemContext
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.firebase.crashlytics.FirebaseCrashlytics
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.consumeEach
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import kotlin.coroutines.EmptyCoroutineContext
import kotlin.time.Duration.Companion.seconds

object RootManager : RootSession(), Logger {
    override val context get() = app.deviceStorage

    @Parcelize
    sealed class RootInitEvent : Parcelable {
        @Parcelize
        class Initialized : RootInitEvent()
        @Parcelize
        data class LoggedThrowable(val priority: Int, val t: ParcelableThrowable) : RootInitEvent()
    }
    @Parcelize
    class RootInit : RootFlow<RootInitEvent> {
        override fun flow() = callbackFlow {
            val channel = Channel<Pair<Int, Throwable>>(Channel.CONFLATED) { (priority, t) ->
                if (priority >= Log.WARN) t.printStackTrace(System.err)
            }
            var logThrowable = { priority: Int, t: Throwable ->
                channel.trySend(priority to t).exceptionOrNull()?.printStackTrace(System.err)
            }
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
            val job = launch {
                channel.consumeEach { (priority, t) ->
                    send(RootInitEvent.LoggedThrowable(priority, ParcelableThrowable(t)))
                }
            }
            send(RootInitEvent.Initialized())
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

    override val rootLifecycleCoroutineContext get() = EmptyCoroutineContext
    override suspend fun handleRootLifecycle(rootProcess: RootProcess) = try {
        FirebaseCrashlytics.getInstance().apply {
            setCustomKey("root.connectedGid", rootProcess.peerCredentials.gid)
            setCustomKey("root.connectedPid", rootProcess.peerCredentials.pid)
            setCustomKey("root.launchedPid", rootProcess.process.pid)
        }
        super.handleRootLifecycle(rootProcess)
        unexpectedLifecycle("stdout/stderr closed unexpectedly")
    } finally {
        GlobalScope.launch {
            var exit = withTimeoutOrNull(10.seconds) { rootProcess.process.awaitExit() }
            if (exit == null) {
                rootProcess.process.destroy()
                exit = withTimeoutOrNull(5.seconds) { rootProcess.process.awaitExit() }
                unexpectedLifecycle(if (exit == null) {
                    rootProcess.process.destroyForcibly()
                    "Root JVM refused to exit"
                } else "Root JVM exited with $exit and timeout")
            } else if (exit != 0) unexpectedLifecycle("Root JVM unexpectedly exited with $exit")
        }
    }
    private fun unexpectedLifecycle(message: String) {
        Timber.w(Exception(message))
        SmartSnackbar.make(message).show()
    }

    override suspend fun initServer(server: RootServer) {
        Logger.me = this
        UnblockCentral.openPidFd
        super.initServer(server)
        val initialized = CompletableDeferred<Unit>()
        GlobalScope.launch(start = CoroutineStart.UNDISPATCHED) {
            try {
                server.flow(RootInit()).collect { event ->
                    when (event) {
                        is RootInitEvent.Initialized -> initialized.complete(Unit)
                        is RootInitEvent.LoggedThrowable -> Timber.tag("RootRemote").log(event.priority,
                            event.t.unwrap())
                    }
                }
                initialized.complete(Unit)
            } catch (e: CancellationException) {
                if (!initialized.isCompleted) initialized.cancel(e)
            } catch (e: Exception) {
                if (!initialized.isCompleted) initialized.completeExceptionally(e) else Timber.w(e)
            }
        }
        initialized.await()
    }
}
