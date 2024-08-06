package be.mygod.vpnhotspot.tasker

import android.content.Context
import android.os.Bundle
import androidx.appcompat.app.AppCompatActivity
import be.mygod.vpnhotspot.TetheringService
import com.joaomgcd.taskerpluginlibrary.condition.TaskerPluginRunnerConditionEvent
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfig
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfigHelper
import com.joaomgcd.taskerpluginlibrary.config.TaskerPluginConfigNoInput
import com.joaomgcd.taskerpluginlibrary.input.TaskerInput
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResultCondition
import com.joaomgcd.taskerpluginlibrary.runner.TaskerPluginResultConditionSatisfied

class TetheringEventConfig : AppCompatActivity(), TaskerPluginConfigNoInput {
    override val context: Context
        get() = this

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        TetheringEventHelper(this).apply {
            onCreate()
            finishForTasker()
        }
    }
}

class TetheringEventHelper(config: TaskerPluginConfig<Unit>) : TaskerPluginConfigHelper<Unit, TetheringState, TetheringEventRunner>(config) {
    override val runnerClass: Class<TetheringEventRunner> = TetheringEventRunner::class.java
    override val inputClass: Class<Unit> = Unit::class.java
    override val outputClass: Class<TetheringState> = TetheringState::class.java
}

class TetheringEventRunner : TaskerPluginRunnerConditionEvent<Unit, TetheringState, Unit>() {
    override fun getSatisfiedCondition(
        context: Context,
        input: TaskerInput<Unit>,
        update: Unit?,
    ): TaskerPluginResultCondition<TetheringState> {
        return TaskerPluginResultConditionSatisfied(
            context = context,
            regular = TetheringState(TetheringService.activeTetherTypes),
        )
    }
}
