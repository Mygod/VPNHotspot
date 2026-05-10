package be.mygod.vpnhotspot

import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Parcelable
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.toByteArray
import be.mygod.vpnhotspot.util.toParcelable
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.File
import java.io.FileNotFoundException

class BootReceiver : BroadcastReceiver() {
    companion object {
        const val KEY = "service.autoStart"

        private val componentName by lazy { ComponentName(app, BootReceiver::class.java) }
        private var enabled: Boolean
            get() = app.packageManager.getComponentEnabledSetting(componentName) ==
                    PackageManager.COMPONENT_ENABLED_STATE_ENABLED
            set(value) = app.packageManager.setComponentEnabledSetting(componentName,
                    if (value) PackageManager.COMPONENT_ENABLED_STATE_ENABLED
                    else PackageManager.COMPONENT_ENABLED_STATE_DISABLED, PackageManager.DONT_KILL_APP)
        private val userEnabled get() = app.pref.getBoolean(KEY, false)
        suspend fun onUserSettingUpdated(shouldStart: Boolean) = configMutex.withLock {
            enabled = shouldStart && loadConfigLocked()?.startables?.isEmpty() == false
        }
        private fun onConfigUpdated(isNotEmpty: Boolean) {
            enabled = isNotEmpty && userEnabled
        }

        private const val FILENAME = "bootconfig"
        private val configFile by lazy { File(app.deviceStorage.noBackupFilesDir, FILENAME) }
        private val configMutex = Mutex()
        private suspend fun migrateIfNecessaryLocked() = withContext(Dispatchers.IO) {
            val oldFile = File(app.noBackupFilesDir, FILENAME)
            if (oldFile.canRead()) try {
                if (!configFile.exists()) oldFile.copyTo(configFile)
                if (!oldFile.delete()) oldFile.deleteOnExit()
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
        private suspend fun loadConfigLocked(): Config? {
            migrateIfNecessaryLocked()
            return withContext(Dispatchers.IO) {
                try {
                    DataInputStream(configFile.inputStream()).use {
                        it.readBytes().toParcelable(Config::class.java.classLoader)
                    }
                } catch (_: FileNotFoundException) {
                    null
                } catch (e: Exception) {
                    Timber.w(e, "Boot config corrupted")
                    null
                }
            }
        }
        private suspend fun updateConfig(work: Config.() -> Boolean) = configMutex.withLock {
            val config = loadConfigLocked() ?: Config()
            if (config.work()) withContext(Dispatchers.IO) {
                DataOutputStream(configFile.outputStream()).use { it.write(config.toByteArray()) }
            }
            config
        }

        suspend fun add(key: String, value: Startable) = try {
            updateConfig { startables.put(key, value).let { true } }
            onConfigUpdated(true)
        } catch (e: Exception) {
            Timber.w(e)
        }
        suspend fun delete(key: String) = try {
            onConfigUpdated(updateConfig { startables.remove(key) != null }.startables.isNotEmpty())
        } catch (e: Exception) {
            Timber.w(e)
        }
        suspend inline fun <reified T> add(value: Startable) = add(T::class.java.name, value)
        suspend inline fun <reified T> delete() = delete(T::class.java.name)

        private var started = false
        private suspend fun startIfNecessary() = configMutex.withLock {
            if (started) return@withLock
            val config = loadConfigLocked()
            if (config == null || config.startables.isEmpty()) enabled = false
            else for (startable in config.startables.values) startable.start(app)
            started = true
        }
        suspend fun startIfEnabled() {
            if (!started && userEnabled) startIfNecessary()
        }
    }

    interface Startable : Parcelable {
        fun start(context: Context)
    }

    @Parcelize
    private data class Config(var startables: MutableMap<String, Startable> = mutableMapOf()) : Parcelable

    @OptIn(DelicateCoroutinesApi::class)
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_LOCKED_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED -> {
                val pending = goAsync()
                GlobalScope.launch(Dispatchers.Main.immediate) {
                    try {
                        if (userEnabled) startIfNecessary() else enabled = false
                    } finally {
                        pending.finish()
                    }
                }
            }
        }
    }
}
