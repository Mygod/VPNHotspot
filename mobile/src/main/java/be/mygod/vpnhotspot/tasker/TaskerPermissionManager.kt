package be.mygod.vpnhotspot.tasker

import android.app.Activity
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import com.joaomgcd.taskerpluginlibrary.TaskerPluginConstants
import net.dinglisch.android.tasker.TaskerPlugin

object TaskerPermissionManager {
    /**
     * See also [com.joaomgcd.taskerpluginlibrary.condition.TaskerPluginRunnerCondition.requestQuery].
     */
    fun <TActivityClass : Activity> requestQuery(context: Context, configActivityClass: Class<TActivityClass>,
                                                 permission: String) {
        val intentRequest = Intent(TaskerPluginConstants.ACTION_REQUEST_QUERY).apply {
            addFlags(Intent.FLAG_RECEIVER_FOREGROUND)
            putExtra(TaskerPluginConstants.EXTRA_ACTIVITY, configActivityClass.name)
            TaskerPlugin.Event.addPassThroughMessageID(this)
        }
        val packagesAlreadyHandled = try {
            requestQueryThroughServicesAndGetSuccessPackages(context, intentRequest, permission)
        } catch (ex: Exception) {
            listOf()
        }
        requestQueryThroughBroadcasts(context, intentRequest, packagesAlreadyHandled, permission)
    }

    private fun requestQueryThroughServicesAndGetSuccessPackages(context: Context, intentRequest: Intent,
                                                                 permission: String): List<String> {
        val packageManager = context.packageManager
        val intent = Intent(TaskerPluginConstants.ACTION_REQUEST_QUERY)
        val resolveInfos = packageManager.queryIntentServices(intent, 0)
        val result = arrayListOf<String>()
        resolveInfos.forEach { resolveInfo ->
            val serviceInfo = resolveInfo.serviceInfo
            if (packageManager.checkPermission(permission, serviceInfo.packageName) !=
                PackageManager.PERMISSION_GRANTED) {
                result.add(serviceInfo.packageName)
                return@forEach
            }
            val componentName = ComponentName(serviceInfo.packageName, serviceInfo.name)
            intentRequest.component = componentName
            try{
                context.startService(intentRequest)
                result.add(serviceInfo.packageName)
            }catch (t:Throwable){
                //not successful. Don't add to successes
            }
        }
        return result
    }

    private fun requestQueryThroughBroadcasts(context: Context, intentRequest: Intent, ignorePackages: List<String>,
                                              permission: String) {
        if (ignorePackages.isEmpty()) {
            context.sendBroadcast(intentRequest)
            return
        }
        val packageManager = context.packageManager
        val intent = Intent(TaskerPluginConstants.ACTION_REQUEST_QUERY)
        val resolveInfos = packageManager.queryBroadcastReceivers(intent, 0)
        return resolveInfos.forEach { resolveInfo ->
            val broadcastInfo = resolveInfo.activityInfo
            val applicationInfo = broadcastInfo.applicationInfo
            if (ignorePackages.contains(applicationInfo.packageName)) return@forEach

            val componentName = ComponentName(broadcastInfo.packageName, broadcastInfo.name)
            intentRequest.component = componentName
            context.sendBroadcast(intentRequest, permission)
        }
    }
}
