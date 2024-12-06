package be.mygod.vpnhotspot.tasker

import be.mygod.vpnhotspot.net.TetherType
import com.joaomgcd.taskerpluginlibrary.output.TaskerOutputObject
import com.joaomgcd.taskerpluginlibrary.output.TaskerOutputVariable

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
}