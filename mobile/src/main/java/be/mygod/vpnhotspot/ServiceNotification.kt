package be.mygod.vpnhotspot

import android.app.*
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.App.Companion.app
import java.util.*

object ServiceNotification {
    private const val CHANNEL_ACTIVE = "tethering"
    private const val CHANNEL_INACTIVE = "tethering-inactive"
    private const val NOTIFICATION_ID = 1

    private val deviceCountsMap = WeakHashMap<Service, Map<String, Int>>()
    private val inactiveMap = WeakHashMap<Service, List<String>>()
    private val manager = app.getSystemService<NotificationManager>()!!

    private fun buildNotification(context: Context): Notification {
        val deviceCounts = deviceCountsMap.values.flatMap { it.entries }.sortedBy { it.key }
        val inactive = inactiveMap.values.flatten()
        val isInactive = inactive.isNotEmpty() && deviceCounts.isEmpty()
        val builder = Notification.Builder(context, if (isInactive) CHANNEL_INACTIVE else CHANNEL_ACTIVE).apply {
            setWhen(0)
            setCategory(Notification.CATEGORY_SERVICE)
            setColor(context.resources.getColor(R.color.colorPrimary, context.theme))
            setContentTitle(context.getText(R.string.notification_tethering_title))
            setSmallIcon(R.drawable.ic_quick_settings_tile_on)
            setContentIntent(PendingIntent.getActivity(context, 0, Intent(context, MainActivity::class.java),
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE))
            setVisibility(Notification.VISIBILITY_PUBLIC)
        }
        var lines = deviceCounts.map { (dev, size) ->
            context.resources.getQuantityString(R.plurals.notification_connected_devices, size, size, dev)
        }
        if (inactive.isNotEmpty()) {
            lines += context.getString(R.string.notification_interfaces_inactive, inactive.joinToString())
        }
        return if (lines.size <= 1) builder.setContentText(lines.singleOrNull()).build() else {
            val deviceCount = deviceCounts.sumOf { it.value }
            val interfaceCount = deviceCounts.size + inactive.size
            Notification.BigTextStyle().apply {
                setBuilder(builder.setContentText(context.resources.getQuantityString(
                    R.plurals.notification_connected_devices, deviceCount, deviceCount,
                    context.resources.getQuantityString(R.plurals.notification_interfaces,
                        interfaceCount, interfaceCount))))
                bigText(lines.joinToString("\n"))
            }.build()!!
        }
    }

    fun startForeground(service: Service, deviceCounts: Map<String, Int>, inactive: List<String> = emptyList()) {
        synchronized(this) {
            deviceCountsMap[service] = deviceCounts
            if (inactive.isEmpty()) inactiveMap.remove(service) else inactiveMap[service] = inactive
            if (Build.VERSION.SDK_INT >= 33) {
                service.startForeground(NOTIFICATION_ID, buildNotification(service),
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
            } else service.startForeground(NOTIFICATION_ID, buildNotification(service))
        }
    }
    fun stopForeground(service: Service) = synchronized(this) {
        deviceCountsMap.remove(service) ?: return@synchronized
        val shutdown = deviceCountsMap.isEmpty()
        service.stopForeground(if (shutdown) Service.STOP_FOREGROUND_REMOVE else Service.STOP_FOREGROUND_DETACH)
        if (!shutdown) manager.notify(NOTIFICATION_ID, buildNotification(service))
    }

    fun updateNotificationChannels() {
        manager.createNotificationChannels(listOf(
            NotificationChannel(CHANNEL_ACTIVE,
                app.getText(R.string.notification_channel_tethering), NotificationManager.IMPORTANCE_LOW).apply {
                lockscreenVisibility = Notification.VISIBILITY_PUBLIC
                setShowBadge(false)
            },
            NotificationChannel(CHANNEL_INACTIVE,
                app.getText(R.string.notification_channel_monitor), NotificationManager.IMPORTANCE_LOW).apply {
                lockscreenVisibility = Notification.VISIBILITY_PUBLIC
                setShowBadge(false)
            }))
        // remove old service channels
        manager.deleteNotificationChannel("hotspot")
        manager.deleteNotificationChannel("repeater")
    }
}
