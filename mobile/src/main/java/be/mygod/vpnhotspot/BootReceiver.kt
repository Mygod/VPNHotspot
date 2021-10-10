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
        fun onUserSettingUpdated(shouldStart: Boolean) {
            enabled = shouldStart && try {
                config
            } catch (e: Exception) {
                Timber.w(e)
                null
            }?.startables?.isEmpty() == false
        }
        private fun onConfigUpdated(isNotEmpty: Boolean) {
            enabled = isNotEmpty && app.pref.getBoolean(KEY, false)
        }

        private var started = false

        private const val FILENAME = "bootconfig"
        private val configFile by lazy { File(app.deviceStorage.noBackupFilesDir, FILENAME) }
        private var config: Config?
            get() = try {
                DataInputStream(configFile.inputStream()).use { it.readBytes().toParcelable() }
            } catch (_: FileNotFoundException) {
                null
            }
            set(value) = DataOutputStream(configFile.outputStream()).use { it.write(value.toByteArray()) }

        fun add(key: String, value: Startable) = try {
            synchronized(BootReceiver) {
                val c = config ?: Config()
                c.startables[key] = value
                config = c
            }
            onConfigUpdated(true)
        } catch (e: Exception) {
            Timber.w(e)
        }
        fun delete(key: String) = try {
            onConfigUpdated(synchronized(BootReceiver) {
                val c = config ?: Config()
                c.startables.remove(key)
                config = c
                c
            }.startables.isNotEmpty())
        } catch (e: Exception) {
            Timber.w(e)
        }
        inline fun <reified T> add(value: Startable) = add(T::class.java.name, value)
        inline fun <reified T> delete() = delete(T::class.java.name)

        @RequiresApi(24)
        fun migrateIfNecessary(old: Context, new: Context) {
            val oldFile = File(old.noBackupFilesDir, FILENAME)
            if (oldFile.canRead()) try {
                val newFile = File(new.noBackupFilesDir, FILENAME)
                if (!newFile.exists()) oldFile.copyTo(newFile)
                if (!oldFile.delete()) oldFile.deleteOnExit()
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }

    interface Startable : Parcelable {
        fun start(context: Context)
    }

    @Parcelize
    private data class Config(var startables: MutableMap<String, Startable> = mutableMapOf()) : Parcelable

    override fun onReceive(context: Context, intent: Intent) {
        if (started) return
        val isUpdate = when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_LOCKED_BOOT_COMPLETED -> false
            Intent.ACTION_MY_PACKAGE_REPLACED -> true
            else -> return
        }
        started = true
        val config = try {
            synchronized(BootReceiver) { config }
        } catch (e: Exception) {
            Timber.w(e)
            if (isUpdate) null else return
        }
        if (config == null || config.startables.isEmpty()) {
            enabled = false
        } else for (startable in config.startables.values) startable.start(context)
    }
}
