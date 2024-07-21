package be.mygod.vpnhotspot.tasker

import android.content.Intent
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager.tetheredIfaces
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

    fun toTaskerOutput(): TetheringState {
        return TetheringState(
            wifi = wifi,
            bluetooth = bluetooth,
            usb = usb,
            ethernet = ethernet,
        )
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
        operator fun invoke(intent: Intent): TetheringState {
            return TetheringState(intent.tetheredIfaces?.filterNotNull() ?: listOf())
        }

        operator fun invoke(interfaces: List<String>): TetheringState {
            return TetheringState(interfaces.map { TetherType.ofInterface(it) }.toSet())
        }

        operator fun invoke(types: Set<TetherType>): TetheringState {
            return TetheringState(
                wifi = types.any { it.isWifi },
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