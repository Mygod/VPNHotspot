package be.mygod.vpnhotspot

import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.toByteArray
import be.mygod.librootkotlinx.toParcelable
import be.mygod.vpnhotspot.App.Companion.app
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
        fun onUserSettingUpdated(shouldStart: Boolean) {
            enabled = shouldStart && try {
                config
            } catch (e: Exception) {
                Timber.w(e)
                null
            }?.startables?.isEmpty() == false
        }
        private fun onConfigUpdated(isNotEmpty: Boolean) {
            enabled = isNotEmpty && userEnabled
        }

        private const val FILENAME = "bootconfig"
        private val configFile by lazy { File(app.deviceStorage.noBackupFilesDir, FILENAME) }
        private val config: Config? get() = try {
            DataInputStream(configFile.inputStream()).use { it.readBytes().toParcelable() }
        } catch (_: FileNotFoundException) {
            null
        }
        private fun updateConfig(work: Config.() -> Unit) = synchronized(BootReceiver) {
            val config = try {
                config
            } catch (e: Exception) {
                Timber.i("Boot config corrupted", e)
                null
            } ?: Config()
            config.work()
            DataOutputStream(configFile.outputStream()).use { it.write(config.toByteArray()) }
            config
        }

        fun add(key: String, value: Startable) = try {
            updateConfig { startables[key] = value }
            onConfigUpdated(true)
        } catch (e: Exception) {
            Timber.w(e)
        }
        fun delete(key: String) = try {
            onConfigUpdated(updateConfig { startables.remove(key) }.startables.isNotEmpty())
        } catch (e: Exception) {
            Timber.w(e)
        }
        inline fun <reified T> add(value: Startable) = add(T::class.java.name, value)
        inline fun <reified T> delete() = delete(T::class.java.name)

        @RequiresApi(24)
        fun migrateIfNecessary() {
            val oldFile = File(app.noBackupFilesDir, FILENAME)
            if (oldFile.canRead()) try {
                if (!configFile.exists()) oldFile.copyTo(configFile)
                if (!oldFile.delete()) oldFile.deleteOnExit()
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
        private var started = false
        private fun startIfNecessary() {
            if (started) return
            val config = try {
                synchronized(BootReceiver) { config }
            } catch (e: Exception) {
                Timber.w(e)
                null
            }
            if (config == null || config.startables.isEmpty()) {
                enabled = false
            } else for (startable in config.startables.values) startable.start(app)
            started = true
        }
        fun startIfEnabled() {
            if (!started && userEnabled) startIfNecessary()
        }
    }

    interface Startable : Parcelable {
        fun start(context: Context)
    }

    @Parcelize
    private data class Config(var startables: MutableMap<String, Startable> = mutableMapOf()) : Parcelable

    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_LOCKED_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED -> {
                if (userEnabled) startIfNecessary() else enabled = false
            }
        }
    }
}
