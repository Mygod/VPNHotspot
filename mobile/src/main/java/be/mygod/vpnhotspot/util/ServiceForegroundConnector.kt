package be.mygod.vpnhotspot.util

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import androidx.fragment.app.Fragment
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import kotlin.reflect.KClass

/**
 * owner also needs to be Context/Fragment.
 */
class ServiceForegroundConnector(private val owner: LifecycleOwner, private val connection: ServiceConnection,
                                 private val clazz: KClass<out Service>) : DefaultLifecycleObserver {
    init {
        owner.lifecycle.addObserver(this)
    }

    private val context get() = when (owner) {
        is Context -> owner
        is Fragment -> owner.requireContext()
        else -> throw UnsupportedOperationException("Unsupported owner")
    }

    override fun onStart(owner: LifecycleOwner) {
        val context = context
        context.bindService(Intent(context, clazz.java), connection, Context.BIND_AUTO_CREATE)
    }

    override fun onStop(owner: LifecycleOwner) = context.stopAndUnbind(connection)
}
