package be.mygod.vpnhotspot

import android.annotation.TargetApi
import android.app.*
import android.content.Context
import android.content.Intent
import android.os.Build
import android.support.v4.app.NotificationCompat
import android.support.v4.content.ContextCompat
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.systemService

object ServiceNotification {
    private const val CHANNEL = "tethering"
    private const val CHANNEL_ID = 1

    private val deviceCountsMap = HashMap<Service, Map<String, Int>>()
    private val manager = app.systemService<NotificationManager>()

    private fun buildNotification(context: Context): Notification {
        val builder = NotificationCompat.Builder(context, CHANNEL)
                .setWhen(0)
                .setColor(ContextCompat.getColor(context, R.color.colorPrimary))
                .setContentTitle(context.getText(R.string.notification_tethering_title))
                .setSmallIcon(R.drawable.ic_quick_settings_tile_on)
                .setContentIntent(PendingIntent.getActivity(context, 0,
                        Intent(context, MainActivity::class.java), PendingIntent.FLAG_UPDATE_CURRENT))
                .setVisibility(Notification.VISIBILITY_PUBLIC)
        val deviceCounts = deviceCountsMap.values.flatMap { it.entries }.sortedBy { it.key }
        return when (deviceCounts.size) {
            0 -> builder.build()
            1 -> {
                val (dev, size) = deviceCounts.single()
                builder.setContentText(context.resources.getQuantityString(R.plurals.notification_connected_devices,
                        size, size, dev))
                        .build()
            }
            else -> {
                val deviceCount = deviceCounts.sumBy { it.value }
                NotificationCompat.BigTextStyle(builder
                        .setContentText(context.resources.getQuantityString(R.plurals.notification_connected_devices,
                                deviceCount, deviceCount,
                                context.resources.getQuantityString(R.plurals.notification_interfaces,
                                        deviceCounts.size, deviceCounts.size))))
                        .bigText(deviceCounts.joinToString("\n") { (dev, size) ->
                            context.resources.getQuantityString(R.plurals.notification_connected_devices,
                                    size, size, dev)
                        })
                        .build()
            }
        }
    }

    fun startForeground(service: Service, deviceCounts: Map<String, Int>) {
        deviceCountsMap[service] = deviceCounts
        service.startForeground(CHANNEL_ID, buildNotification(service))
    }
    fun stopForeground(service: Service) {
        deviceCountsMap.remove(service)
        if (deviceCountsMap.isEmpty()) service.stopForeground(true) else {
            service.stopForeground(false)
            manager.notify(CHANNEL_ID, buildNotification(service))
        }
    }

    fun updateNotificationChannels() {
        if (Build.VERSION.SDK_INT >= 26) @TargetApi(26) {
            val tethering = NotificationChannel(CHANNEL,
                    app.getText(R.string.notification_channel_tethering), NotificationManager.IMPORTANCE_LOW)
            tethering.lockscreenVisibility = Notification.VISIBILITY_PUBLIC
            manager.createNotificationChannel(tethering)
            // remove old service channels
            manager.deleteNotificationChannel("hotspot")
            manager.deleteNotificationChannel("repeater")
        }
    }
}
