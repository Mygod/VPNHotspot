package be.mygod.vpnhotspot.util

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob

interface MainScope : CoroutineScope {
    class Supervisor : MainScope {
        override val job = SupervisorJob()
    }
    val job: Job
    override val coroutineContext get() = Dispatchers.Main + job
}
