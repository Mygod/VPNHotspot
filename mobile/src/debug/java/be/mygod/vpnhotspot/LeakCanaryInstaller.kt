package be.mygod.vpnhotspot

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.net.Uri
import android.os.Handler
import android.os.Looper
import com.google.firebase.crashlytics.CustomKeysAndValues
import com.google.firebase.crashlytics.FirebaseCrashlytics
import leakcanary.EventListener
import leakcanary.LeakCanary
import shark.Leak

class LeakCanaryInstaller : ContentProvider() {
    override fun onCreate(): Boolean {
        Handler(Looper.getMainLooper()).post {
            val config = LeakCanary.config
            LeakCanary.config = config.copy(eventListeners = config.eventListeners + EventListener { event ->
                when (event) {
                    is EventListener.Event.HeapAnalysisDone.HeapAnalysisSucceeded -> {
                        val heapAnalysis = event.heapAnalysis
                        val appLeaks: List<Leak> = heapAnalysis.applicationLeaks
                        val libLeaks: List<Leak> = heapAnalysis.libraryLeaks
                        for ((type, leaks) in sequenceOf("application" to appLeaks, "library" to libLeaks)) {
                            for (leak in leaks) {
                                val exception = IllegalStateException(
                                    "LeakCanary $type leak: ${leak.shortDescription} [${leak.signature}]")
                                exception.stackTrace = arrayOf(StackTraceElement("LeakCanary", "${type}Leak",
                                    leak.signature.take(255), 1))
                                FirebaseCrashlytics.getInstance().recordException(exception,
                                    CustomKeysAndValues.Builder().apply {
                                        putString("leakcanary.analysis", "success")
                                        putString("leakcanary.type", type)
                                        putString("leakcanary.signature", leak.signature.take(1024))
                                        putInt("leakcanary.applicationLeakCount", appLeaks.size)
                                        putInt("leakcanary.libraryLeakCount", libLeaks.size)
                                        putInt("leakcanary.traceCount", leak.leakTraces.size)
                                        leak.totalRetainedHeapByteSize?.let {
                                            putLong("leakcanary.retainedHeapByteSize", it.toLong())
                                        }
                                        leak.totalRetainedObjectCount?.let {
                                            putLong("leakcanary.retainedObjectCount", it.toLong())
                                        }
                                        putString("leakcanary.heapDumpFile", heapAnalysis.heapDumpFile.name.take(1024))
                                        putLong("leakcanary.dumpDurationMillis", heapAnalysis.dumpDurationMillis)
                                        putLong("leakcanary.analysisDurationMillis",
                                            heapAnalysis.analysisDurationMillis)
                                    }.build())
                            }
                        }
                    }

                    is EventListener.Event.HeapAnalysisDone.HeapAnalysisFailed -> {
                        val heapAnalysis = event.heapAnalysis
                        FirebaseCrashlytics.getInstance().recordException(heapAnalysis.exception,
                            CustomKeysAndValues.Builder().apply {
                                putString("leakcanary.analysis", "failure")
                                putString("leakcanary.heapDumpFile", heapAnalysis.heapDumpFile.name.take(1024))
                                putLong("leakcanary.dumpDurationMillis", heapAnalysis.dumpDurationMillis)
                                putLong("leakcanary.analysisDurationMillis", heapAnalysis.analysisDurationMillis)
                            }.build())
                    }

                    else -> Unit
                }
            })
        }
        return true
    }

    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?) = 0
    override fun getType(uri: Uri): String? = null
    override fun insert(uri: Uri, values: ContentValues?): Uri? = null
    override fun query(
        uri: Uri,
        projection: Array<out String>?,
        selection: String?,
        selectionArgs: Array<out String>?,
        sortOrder: String?,
    ): Cursor? = null
    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?) = 0
}
