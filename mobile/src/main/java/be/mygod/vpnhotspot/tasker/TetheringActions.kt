package be.mygod.vpnhotspot.tasker

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.TetheringManager
import android.os.Build
import android.os.Bundle
import androidx.annotation.RequiresApi
import androidx.appcompat.app.AppCompatActivity
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import com.joaomgcd.taskerpluginlibrary.action.TaskerPluginRunnerActionNoOutput
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfig
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfigHelperNoOutput
import com.joaomgcd.taskerpluginlibrary.input.TaskerInput
import com.joaomgcd.taskerpluginlibrary.input.TaskerInputField
import com.joaomgcd.taskerpluginlibrary.input.TaskerInputRoot
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResult
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResultError
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResultSucess
import kotlinx.coroutines.runBlocking
import kotlin.coroutines.resume
import kotlin.coroutines.suspendCoroutine

@TaskerInputRoot
data class ActionInput(
    @TaskerInputField("tetherType")
    var tetherType: Int = -1,
    @TaskerInputField("enable")
    var enable: Boolean = false,
)

sealed class ActionConfig(tetherType: Int) : AppCompatActivity(), TaskerPluginConfig<ActionInput> {
    override val context by lazy { this }
    override val inputForTasker: TaskerInput<ActionInput>
        get() = TaskerInput(input)

    private val helper by lazy { ActionHelper(this) }

    private var input: ActionInput = ActionInput(
        enable = false,
        tetherType = tetherType,
    )

    override fun assignFromInput(input: TaskerInput<ActionInput>) {
        this.input = this.input.copy(enable = !input.regular.enable)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        helper.onCreate()
        helper.finishForTasker()
    }

    class WifiConfig : ActionConfig(TetheringManager.TETHERING_WIFI)
    class USBConfig : ActionConfig(TetheringManagerCompat.TETHERING_USB)
    class BluetoothConfig : ActionConfig(TetheringManagerCompat.TETHERING_BLUETOOTH)
    @RequiresApi(Build.VERSION_CODES.R)
    class EthernetConfig : ActionConfig(TetheringManagerCompat.TETHERING_ETHERNET)
}

class ActionHelper(
    config: TaskerPluginConfig<ActionInput>,
) : TaskerPluginConfigHelperNoOutput<ActionInput, ActionRunner>(config) {
    override val inputClass: Class<ActionInput> = ActionInput::class.java
    override val runnerClass: Class<ActionRunner> = ActionRunner::class.java
    override val addDefaultStringBlurb: Boolean = false

    override fun addToStringBlurb(input: TaskerInput<ActionInput>, blurbBuilder: StringBuilder) {
        blurbBuilder.appendLine("enable: ${input.regular.enable}")
    }
}

class ActionRunner : TaskerPluginRunnerActionNoOutput<ActionInput>() {
    override fun run(context: Context, input: TaskerInput<ActionInput>): TaskerPluginResult<Unit> {
        if (context.checkCallingPermission("android.permission.TETHER_PRIVILEGED") !=
            PackageManager.PERMISSION_GRANTED &&
            context.checkCallingPermission(Manifest.permission.WRITE_SETTINGS) != PackageManager.PERMISSION_GRANTED) {
            return TaskerPluginResultError(SecurityException("Need TETHER_PRIVILEGED or WRITE_SETTINGS permission"))
        }
        return runBlocking {
            suspendCoroutine { continuation ->
                try {
                    if (input.regular.enable) {
                        TetheringManagerCompat.startTethering(input.regular.tetherType, true, object : TetheringManagerCompat.StartTetheringCallback {
                            override fun onTetheringStarted() {
                                continuation.resume(TaskerPluginResultSucess())
                            }

                            override fun onException(e: Exception) {
                                continuation.resume(TaskerPluginResultError(e))
                            }

                            override fun onTetheringFailed(error: Int?) {
                                continuation.resume(TaskerPluginResultError(error ?: -1, "Tethering failed to start."))
                            }
                        })
                    } else {
                        TetheringManagerCompat.stopTethering(input.regular.tetherType, object : TetheringManagerCompat.StopTetheringCallback {
                            override fun onStopTetheringSucceeded() {
                                continuation.resume(TaskerPluginResultSucess())
                            }

                            override fun onStopTetheringFailed(error: Int) {
                                continuation.resume(TaskerPluginResultError(error, "Tethering failed to stop."))
                            }

                            override fun onException(e: Exception) {
                                continuation.resume(TaskerPluginResultError(e))
                            }
                        })
                    }
                } catch (e: Throwable) {
                    continuation.resume(TaskerPluginResultError(e))
                }
            }
        }
    }
}
