package be.mygod.vpnhotspot.util

import android.app.Activity
import android.net.Uri
import be.mygod.vpnhotspot.App.Companion.app
import com.google.android.play.core.appupdate.AppUpdateManagerFactory
import com.google.android.play.core.install.InstallException
import com.google.android.play.core.install.model.InstallErrorCode
import com.google.android.play.core.install.model.InstallStatus
import com.google.android.play.core.ktx.AppUpdateResult
import com.google.android.play.core.ktx.requestUpdateFlow
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import timber.log.Timber

object UpdateChecker {
    private class UpdateAvailable(private val update: AppUpdateResult.Available) : AppUpdate {
        override val stalenessDays get() = update.updateInfo.clientVersionStalenessDays() ?: 0
        override fun updateForResult(activity: Activity, requestCode: Int) = try {
            check(update.startFlexibleUpdate(activity, requestCode)) { "startFlexibleUpdate failed" }
        } catch (e: Exception) {
            Timber.w(e)
            app.customTabsIntent.launchUrl(activity,
                Uri.parse("https://play.google.com/store/apps/details?id=be.mygod.vpnhotspot"))
        }
    }
    private class UpdateDownloading(private val update: AppUpdateResult.InProgress) : AppUpdate {
        override val downloaded get() = false
        override val message: String? get() {
            if (update.installState.installStatus() != InstallStatus.FAILED) return null
            val code = update.installState.installErrorCode()
            for (f in InstallErrorCode::class.java.declaredFields) if (f.getInt(null) == code) return f.name
            return "Unrecognized Error"
        }
    }
    private class UpdateDownloaded(private val update: AppUpdateResult.Downloaded) : AppUpdate {
        override val downloaded get() = true
        override val stalenessDays get() = 0
        override fun updateForResult(activity: Activity, requestCode: Int) {
            GlobalScope.launch { update.completeUpdate() }
        }
    }

    private val manager by lazy { AppUpdateManagerFactory.create(app) }

    fun check() = manager.requestUpdateFlow().catch { e ->
        when (e) {
            is InstallException -> {
                app.logEvent("InstallErrorCode") { param("errorCode", e.errorCode.toLong()) }
                throw AppUpdate.IgnoredException(e)
            }
            is RuntimeException -> if (e.message == "Failed to bind to the service.") {
                app.logEvent("UpdateBindFailure")
                throw AppUpdate.IgnoredException(e)
            }
        }
        throw e
    }.map { result ->
        when (result) {
            is AppUpdateResult.NotAvailable -> null
            is AppUpdateResult.Available -> UpdateAvailable(result)
            is AppUpdateResult.InProgress -> {
                if (result.installState.installStatus() == InstallStatus.CANCELED) null else UpdateDownloading(result)
            }
            is AppUpdateResult.Downloaded -> UpdateDownloaded(result)
        }
    }
}
