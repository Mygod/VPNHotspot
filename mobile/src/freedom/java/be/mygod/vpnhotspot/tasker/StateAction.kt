package be.mygod.vpnhotspot.tasker

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.appcompat.app.AppCompatActivity
import be.mygod.vpnhotspot.TetheringService
import com.joaomgcd.taskerpluginlibrary.action.TaskerPluginRunnerActionNoInput
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfigHelperNoInput
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfigNoInput
import com.joaomgcd.taskerpluginlibrary.input.TaskerInput
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResultErrorWithOutput
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResultSucess

class GetStateConfig : AppCompatActivity(), TaskerPluginConfigNoInput {
    override val context: Context
        get() = this

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        GetStateHelper(this).apply {
            onCreate()
            finishForTasker()
        }
    }
}

class GetStateHelper(config: GetStateConfig) : TaskerPluginConfigHelperNoInput<TetheringState, GetStateRunner>(config) {
    override val outputClass: Class<TetheringState> = TetheringState::class.java
    override val runnerClass: Class<GetStateRunner> = GetStateRunner::class.java
}

class GetStateRunner : TaskerPluginRunnerActionNoInput<TetheringState>() {
    override fun run(
        context: Context,
        input: TaskerInput<Unit>,
    ) = if (context.checkCallingPermission(Manifest.permission.ACCESS_NETWORK_STATE) ==
        PackageManager.PERMISSION_GRANTED) {
        TaskerPluginResultSucess(TetheringState(TetheringService.activeTetherTypes))
    } else TaskerPluginResultErrorWithOutput(SecurityException("Need ACCESS_NETWORK_STATE permission"))
}
