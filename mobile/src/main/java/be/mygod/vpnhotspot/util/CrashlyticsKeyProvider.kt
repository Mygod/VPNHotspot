package be.mygod.vpnhotspot.util

import com.google.firebase.crashlytics.CustomKeysAndValues

interface CrashlyticsKeyProvider {
    val crashlyticsKeys: CustomKeysAndValues
}
