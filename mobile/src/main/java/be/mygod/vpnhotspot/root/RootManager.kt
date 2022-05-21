package be.mygod.vpnhotspot.root

import android.annotation.SuppressLint
import android.os.Build
import android.os.Parcelable
import android.util.Log
import be.mygod.librootkotlinx.*
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import com.google.firebase.crashlytics.FirebaseCrashlytics
import kotlinx.parcelize.Parcelize
import timber.log.Timber

object RootManager : RootSession(), Logger {
    @Parcelize
    class RootInit : RootCommandNoResult {
        override suspend fun execute(): Parcelable? {
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
                        if (priority >= Log.WARN) t.printStackTrace(System.err)
                    }
                }
            })
            Logger.me = RootManager
            Services.init { systemContext }
            if (Build.VERSION.SDK_INT >= 28) UnblockCentral.needInit = false
            return null
        }
    }

    override fun d(m: String?, t: Throwable?) = Timber.d(t, m)
    override fun e(m: String?, t: Throwable?) = Timber.e(t, m)
    override fun i(m: String?, t: Throwable?) = Timber.i(t, m)
    override fun w(m: String?, t: Throwable?) = Timber.w(t, m)

    override suspend fun initServer(server: RootServer) {
        Logger.me = this
        AppProcess.shouldRelocateHeuristics.let {
            FirebaseCrashlytics.getInstance().setCustomKey("RootManager.relocateEnabled", it)
            server.init(app.deviceStorage, it)
        }
        server.execute(RootInit())
    }
}
