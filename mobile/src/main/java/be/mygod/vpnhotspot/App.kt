package be.mygod.vpnhotspot

import android.annotation.SuppressLint
import android.app.Application
import android.app.UiModeManager
import android.content.ClipboardManager
import android.content.res.Configuration
import android.net.ConnectivityManager
import android.net.wifi.WifiManager
import android.os.Build
import android.util.Log
import androidx.annotation.Size
import androidx.browser.customtabs.CustomTabColorSchemeParams
import androidx.browser.customtabs.CustomTabsIntent
import androidx.core.content.ContextCompat
import androidx.core.content.getSystemService
import androidx.core.provider.FontRequest
import androidx.emoji.text.EmojiCompat
import androidx.emoji.text.FontRequestEmojiCompatConfig
import androidx.preference.PreferenceManager
import be.mygod.vpnhotspot.net.DhcpWorkaround
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.util.DeviceStorageApp
import be.mygod.vpnhotspot.util.RootSession
import com.google.firebase.analytics.ktx.ParametersBuilder
import com.google.firebase.analytics.ktx.analytics
import com.google.firebase.crashlytics.FirebaseCrashlytics
import com.google.firebase.ktx.Firebase
import com.google.firebase.ktx.initialize
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.util.*

class App : Application() {
    companion object {
        @SuppressLint("StaticFieldLeak")
        lateinit var app: App
    }

    override fun onCreate() {
        super.onCreate()
        app = this
        if (Build.VERSION.SDK_INT >= 24) @SuppressLint("RestrictedApi") {
            deviceStorage = DeviceStorageApp(this)
            // alternative to PreferenceManager.getDefaultSharedPreferencesName(this)
            deviceStorage.moveSharedPreferencesFrom(this, PreferenceManager(this).sharedPreferencesName)
            deviceStorage.moveDatabaseFrom(this, AppDatabase.DB_NAME)
        } else deviceStorage = this
        Firebase.initialize(deviceStorage)
        Timber.plant(object : Timber.DebugTree() {
            override fun log(priority: Int, tag: String?, message: String, t: Throwable?) {
                if (t == null) {
                    if (priority != Log.DEBUG || BuildConfig.DEBUG) Log.println(priority, tag, message)
                    FirebaseCrashlytics.getInstance().log("${"XXVDIWEF".getOrElse(priority) { 'X' }}/$tag: $message")
                } else {
                    if (priority >= Log.WARN || priority == Log.DEBUG) Log.println(priority, tag, message)
                    if (priority >= Log.INFO) FirebaseCrashlytics.getInstance().recordException(t)
                }
            }
        })
        ServiceNotification.updateNotificationChannels()
        EmojiCompat.init(FontRequestEmojiCompatConfig(deviceStorage, FontRequest(
                "com.google.android.gms.fonts",
                "com.google.android.gms",
                "Noto Color Emoji Compat",
                R.array.com_google_android_gms_fonts_certs)).apply {
            setEmojiSpanIndicatorEnabled(BuildConfig.DEBUG)
            registerInitCallback(object : EmojiCompat.InitCallback() {
                override fun onInitialized() = Timber.d("EmojiCompat initialized")
                override fun onFailed(throwable: Throwable?) = Timber.d(throwable)
            })
        })
        EBegFragment.init()
        if (DhcpWorkaround.shouldEnable) DhcpWorkaround.enable(true)
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        ServiceNotification.updateNotificationChannels()
    }

    override fun onTrimMemory(level: Int) {
        super.onTrimMemory(level)
        if (level >= TRIM_MEMORY_RUNNING_CRITICAL) GlobalScope.launch { RootSession.trimMemory() }
    }

    /**
     * This method is used to log "expected" and well-handled errors, i.e. we care less about logs, etc.
     * logException is inappropriate sometimes because it flushes all logs that could be used to investigate other bugs.
     */
    fun logEvent(@Size(min = 1L, max = 40L) event: String, block: ParametersBuilder.() -> Unit = { }) {
        val builder = ParametersBuilder()
        builder.block()
        Timber.i(if (builder.bundle.isEmpty) event else "$event, extras: ${builder.bundle}")
        Firebase.analytics.logEvent(event, builder.bundle)
    }

    lateinit var deviceStorage: Application
    val english by lazy {
        createConfigurationContext(Configuration(resources.configuration).apply {
            setLocale(Locale.ENGLISH)
        })
    }
    val pref by lazy { PreferenceManager.getDefaultSharedPreferences(deviceStorage) }
    val connectivity by lazy { getSystemService<ConnectivityManager>()!! }
    val clipboard by lazy { getSystemService<ClipboardManager>()!! }
    val uiMode by lazy { getSystemService<UiModeManager>()!! }
    val wifi by lazy { getSystemService<WifiManager>()!! }

    val hasTouch by lazy { packageManager.hasSystemFeature("android.hardware.faketouch") }
    val customTabsIntent by lazy {
        CustomTabsIntent.Builder().apply {
            setColorScheme(CustomTabsIntent.COLOR_SCHEME_SYSTEM)
            setColorSchemeParams(CustomTabsIntent.COLOR_SCHEME_LIGHT, CustomTabColorSchemeParams.Builder().apply {
                setToolbarColor(ContextCompat.getColor(app, R.color.light_colorPrimary))
            }.build())
            setColorSchemeParams(CustomTabsIntent.COLOR_SCHEME_DARK, CustomTabColorSchemeParams.Builder().apply {
                setToolbarColor(ContextCompat.getColor(app, R.color.dark_colorPrimary))
            }.build())
        }.build()
    }
}
