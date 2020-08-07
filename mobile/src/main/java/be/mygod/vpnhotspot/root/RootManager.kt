package be.mygod.vpnhotspot.root

import android.os.Parcelable
import android.util.Log
import be.mygod.librootkotlinx.*
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import kotlinx.android.parcel.Parcelize
import timber.log.Timber

object RootManager : RootSession(), Logger {
    @Parcelize
    class RootInit : RootCommandNoResult {
        override suspend fun execute(): Parcelable? {
            Timber.plant(object : Timber.DebugTree() {
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
            return null
        }
    }

    override fun d(m: String?, t: Throwable?) = Timber.d(t, m)
    override fun e(m: String?, t: Throwable?) = Timber.e(t, m)
    override fun i(m: String?, t: Throwable?) = Timber.i(t, m)
    override fun w(m: String?, t: Throwable?) = Timber.w(t, m)

    override suspend fun initServer(server: RootServer) {
        Logger.me = this
        server.init(app.deviceStorage)
        server.execute(RootInit())
    }
}
