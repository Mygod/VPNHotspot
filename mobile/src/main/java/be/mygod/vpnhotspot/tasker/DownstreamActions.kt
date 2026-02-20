package be.mygod.vpnhotspot.tasker

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.ServiceConnection
import android.net.TetheringManager
import android.os.Bundle
import android.os.IBinder
import androidx.appcompat.app.AppCompatActivity
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.TetheringManagerCompat.tetheredIfaces
import com.joaomgcd.taskerpluginlibrary.action.TaskerPluginRunnerActionNoOutput
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfig
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfigHelperNoOutput
import com.joaomgcd.taskerpluginlibrary.input.TaskerInput
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResult
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResultError
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResultSucess
import kotlinx.coroutines.runBlocking
import kotlin.coroutines.resume
import kotlin.coroutines.suspendCoroutine

sealed class DownstreamActionConfig(tetherType: Int) : AppCompatActivity(), TaskerPluginConfig<ActionInput> {
    override val context by lazy { this }
    override val inputForTasker: TaskerInput<ActionInput>
        get() = TaskerInput(input)

    private val helper by lazy { DownstreamActionHelper(this) }

    private var input: ActionInput = ActionInput(
        enable = false,
        tetherType = tetherType
    )

    override fun assignFromInput(input: TaskerInput<ActionInput>) {
        this.input = this.input.copy(enable = !input.regular.enable)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        helper.onCreate()
        helper.finishForTasker()
    }

    class WifiConfig : DownstreamActionConfig(TetheringManager.TETHERING_WIFI)
    class USBConfig : DownstreamActionConfig(TetheringManagerCompat.TETHERING_USB)
    class BluetoothConfig : DownstreamActionConfig(TetheringManagerCompat.TETHERING_BLUETOOTH)
    class EthernetConfig : DownstreamActionConfig(TetheringManagerCompat.TETHERING_ETHERNET)
}

class DownstreamActionHelper(
    config: TaskerPluginConfig<ActionInput>,
) : TaskerPluginConfigHelperNoOutput<ActionInput, DownstreamActionRunner>(config) {
    override val inputClass: Class<ActionInput> = ActionInput::class.java
    override val runnerClass: Class<DownstreamActionRunner> = DownstreamActionRunner::class.java
    override val addDefaultStringBlurb: Boolean = false

    override fun addToStringBlurb(input: TaskerInput<ActionInput>, blurbBuilder: StringBuilder) {
        blurbBuilder.appendLine("enable: ${input.regular.enable}")
    }
}

class DownstreamActionRunner : TaskerPluginRunnerActionNoOutput<ActionInput>(), ServiceConnection {
    private var binder: TetheringService.Binder? = null

    override fun onServiceConnected(name: ComponentName, service: IBinder) {
        binder = service as TetheringService.Binder
    }

    override fun onServiceDisconnected(name: ComponentName) {
        binder = null
    }

    override fun run(context: Context, input: TaskerInput<ActionInput>): TaskerPluginResult<Unit> {
        return runBlocking {
            suspendCoroutine { continuation ->
                try {
                    val intent = Intent(context, TetheringService::class.java)
                    context.bindService(intent, this@DownstreamActionRunner, Context.BIND_AUTO_CREATE)
                    val tetherType = TetherType.fromTetheringType(input.regular.tetherType)
                    val tetheredOfType = context.registerReceiver(null, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))?.tetheredIfaces?.filter { TetherType.ofInterface(it).isA(tetherType) }
                    if (tetheredOfType?.isEmpty() == true) {
                        throw Exception("No interface to route to")
                    }
                    Thread.sleep(1000)
                    val binder = binder
                    if (binder == null) {
                        throw Exception("Couldn't get TetheringService binder")
                    }
                    if (input.regular.enable) {
                        val inactive = tetheredOfType?.filterNot(binder::isActive)
                        context.startForegroundService(Intent(context, TetheringService::class.java)
                            .putExtra(TetheringService.EXTRA_ADD_INTERFACES, inactive?.toTypedArray()))
                        continuation.resume(TaskerPluginResultSucess())
                    } else {
                        val active = tetheredOfType?.filter(binder::isActive)
                        for (iface in active!!) {
                            context.startForegroundService(Intent(context, TetheringService::class.java)
                                    .putExtra(TetheringService.EXTRA_REMOVE_INTERFACE, iface))
                        }
                        continuation.resume(TaskerPluginResultSucess())
                    }
                } catch (e: Throwable) {
                    continuation.resume(TaskerPluginResultError(e))
                }
            }
        }
    }
}
