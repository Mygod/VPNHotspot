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
 * owner also needs to be Context/Fragment.
 */
class ServiceForegroundConnector(private val owner: LifecycleOwner, private val connection: ServiceConnection,
                                 private val clazz: KClass<out Service>) : LifecycleObserver {
    init {
        owner.lifecycle.addObserver(this)
    }

    private val context get() = when (owner) {
        is Context -> owner
        is Fragment -> owner.requireContext()
        else -> throw UnsupportedOperationException("Unsupported owner")
    }

    @OnLifecycleEvent(Lifecycle.Event.ON_START)
    fun onStart() {
        val context = context
        context.bindService(Intent(context, clazz.java), connection, Context.BIND_AUTO_CREATE)
    }

    @OnLifecycleEvent(Lifecycle.Event.ON_STOP)
    fun onStop() = context.stopAndUnbind(connection)
}
