package be.mygod.vpnhotspot.tasker

import be.mygod.vpnhotspot.net.TetherType
import com.joaomgcd.taskerpluginlibrary.input.TaskerInputField
import com.joaomgcd.taskerpluginlibrary.input.TaskerInputRoot
import com.joaomgcd.taskerpluginlibrary.output.TaskerOutputObject
import com.joaomgcd.taskerpluginlibrary.output.TaskerOutputVariable

@TaskerInputRoot
class TetheringStateInput(
    @TaskerInputField("wifi")
    var wifi: Boolean = false,
    @TaskerInputField("bluetooth")
    var bluetooth: Boolean = false,
    @TaskerInputField("usb")
    var usb: Boolean = false,
    @TaskerInputField("ethernet")
    var ethernet: Boolean = false,
) {
    companion object {
        operator fun invoke(state: TetheringState): TetheringStateInput {
            return TetheringStateInput(
                wifi = state.wifi,
                bluetooth = state.bluetooth,
                usb = state.usb,
                ethernet = state.ethernet,
            )
        }
    }
}

@TaskerOutputObject
class TetheringState(
    @get:TaskerOutputVariable("wifi")
    val wifi: Boolean = false,
    @get:TaskerOutputVariable("bluetooth")
    val bluetooth: Boolean = false,
    @get:TaskerOutputVariable("usb")
    val usb: Boolean = false,
    @get:TaskerOutputVariable("ethernet")
    val ethernet: Boolean = false,
) {
    companion object {
        operator fun invoke(types: Set<TetherType>): TetheringState {
            return TetheringState(
                wifi = types.contains(TetherType.WIFI),
                bluetooth = types.contains(TetherType.BLUETOOTH),
                usb = types.contains(TetherType.USB) || types.contains(TetherType.NCM),
                ethernet = types.contains(TetherType.ETHERNET),
            )
        }
    }

    fun toTaskerInput(): TetheringStateInput {
        return TetheringStateInput(this)
    }
}