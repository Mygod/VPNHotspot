package be.mygod.vpnhotspot.util

import android.app.Service
import android.arch.lifecycle.Lifecycle
import android.arch.lifecycle.LifecycleObserver
import android.arch.lifecycle.LifecycleOwner
import android.arch.lifecycle.OnLifecycleEvent
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.support.v4.app.Fragment
import kotlin.reflect.KClass

/**
 * host also needs to be Context/Fragment and LifecycleOwner.
 */
class ServiceForegroundConnector(private val host: ServiceConnection, private val classes: List<KClass<out Service>>) :
        LifecycleObserver {
    init {
        (host as LifecycleOwner).lifecycle.addObserver(this)
    }
    constructor(host: ServiceConnection, vararg classes: KClass<out Service>) : this(host, classes.toList())

    private val context get() = if (host is Context) host else (host as Fragment).requireContext()

    @OnLifecycleEvent(Lifecycle.Event.ON_START)
    fun onStart() {
        val context = context
        for (clazz in classes) context.bindService(Intent(context, clazz.java), host, Context.BIND_AUTO_CREATE)
    }

    @OnLifecycleEvent(Lifecycle.Event.ON_STOP)
    fun onStop() = context.stopAndUnbind(host)
}
