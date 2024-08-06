package be.mygod.vpnhotspot.tasker

import android.content.Context
import android.os.Bundle
import androidx.appcompat.app.AppCompatActivity
import be.mygod.vpnhotspot.TetheringService
import com.joaomgcd.taskerpluginlibrary.action.TaskerPluginRunnerActionNoInput
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfigHelperNoInput
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfigNoInput
import com.joaomgcd.taskerpluginlibrary.input.TaskerInput
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
    ) = TaskerPluginResultSucess(TetheringState(TetheringService.activeTetherTypes))
}
