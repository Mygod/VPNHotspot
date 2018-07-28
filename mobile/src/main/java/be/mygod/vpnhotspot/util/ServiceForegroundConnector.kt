package be.mygod.vpnhotspot.util

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import androidx.fragment.app.Fragment
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.OnLifecycleEvent
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
