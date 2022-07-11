package be.mygod.vpnhotspot.manage

import android.Manifest
import android.annotation.TargetApi
import android.bluetooth.BluetoothAdapter
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Build
import android.os.Parcelable
import android.provider.Settings
import android.text.SpannableStringBuilder
import android.text.format.DateUtils
import android.view.View
import android.widget.Toast
import androidx.annotation.RequiresApi
import androidx.core.net.toUri
import androidx.core.view.updatePaddingRelative
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.MainActivity
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.wifi.*
import be.mygod.vpnhotspot.net.wifi.WifiApManager.wifiApState
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.*
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.util.*

sealed class TetherManager(protected val parent: TetheringFragment) : Manager(),
        TetheringManager.StartTetheringCallback {
    class ViewHolder(private val binding: ListitemInterfaceBinding) : RecyclerView.ViewHolder(binding.root),
            View.OnClickListener {
        init {
            itemView.updatePaddingRelative(start = itemView.resources.getDimensionPixelOffset(
                    R.dimen.listitem_manage_tether_padding_start))
            itemView.setOnClickListener(this)
        }

        var manager: TetherManager? = null
            set(value) {
                field = value!!
                binding.data = value.data
            }

        override fun onClick(v: View?) {
            val manager = manager!!
            val mainActivity = manager.parent.activity as MainActivity
            if (Build.VERSION.SDK_INT >= 23 && !Settings.System.canWrite(mainActivity)) try {
                manager.parent.startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS,
                        "package:${mainActivity.packageName}".toUri()))
                return
            } catch (e: RuntimeException) {
                app.logEvent("manage_write_settings") { param("message", e.toString()) }
            }
            when (manager.isStarted) {
                true -> try {
                    manager.stop()
                } catch (e: InvocationTargetException) {
                    if (e.targetException !is SecurityException) Timber.w(e)
                    manager.onException(e)
                }
                false -> manager.start()
                null -> manager.onClickNull()
            }
        }
    }

    /**
     * A convenient class to delegate stuff to BaseObservable.
     */
    inner class Data : be.mygod.vpnhotspot.manage.Data() {
        override val icon get() = tetherType.icon
        override val title get() = this@TetherManager.title
        override val text get() = this@TetherManager.text
        override val active get() = isStarted == true
    }

    val data = Data()
    abstract val title: CharSequence
    abstract val tetherType: TetherType
    open val isStarted: Boolean? get() = parent.enabledTypes.contains(tetherType)
    protected open val text: CharSequence get() = baseError ?: ""

    protected var baseError: String? = null
        private set

    protected abstract fun start()
    protected abstract fun stop()
    protected open fun onClickNull(): Unit = throw UnsupportedOperationException()

    override fun onTetheringStarted() = data.notifyChange()
    override fun onTetheringFailed(error: Int?) {
        Timber.d("onTetheringFailed: $error")
        if (Build.VERSION.SDK_INT < 30 || error != TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) {
            error?.let { SmartSnackbar.make("$tetherType: ${TetheringManager.tetherErrorLookup(it)}").show() }
        } else GlobalScope.launch(Dispatchers.Main.immediate) {
            val context = parent.context ?: app
            Toast.makeText(context, R.string.permission_missing, Toast.LENGTH_LONG).show()
            ManageBar.start(context)
        }
        data.notifyChange()
    }
    override fun onException(e: Exception) {
        super.onException(e)
        GlobalScope.launch(Dispatchers.Main.immediate) {
            val context = parent.context ?: app
            Toast.makeText(context, e.readableMessage, Toast.LENGTH_LONG).show()
            ManageBar.start(context)
        }
    }

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        (viewHolder as ViewHolder).manager = this
    }

    fun updateErrorMessage(errored: List<String>, lastErrors: Map<String, Int>) {
        val interested = errored.filter { TetherType.ofInterface(it) == tetherType }
        baseError = if (interested.isEmpty()) null else interested.joinToString("\n") { iface ->
            "$iface: " + try {
                TetheringManager.tetherErrorLookup(if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                    TetheringManager.getLastTetherError(iface)
                } else lastErrors[iface] ?: 0)
            } catch (e: InvocationTargetException) {
                if (Build.VERSION.SDK_INT !in 24..25 || e.cause !is SecurityException) Timber.w(e) else Timber.d(e)
                e.readableMessage
            }
        }
        data.notifyChange()
    }

    @RequiresApi(24)
    class Wifi(parent: TetheringFragment) : TetherManager(parent), DefaultLifecycleObserver,
            WifiApManager.SoftApCallbackCompat {
        private val receiver = broadcastReceiver { _, intent ->
            failureReason = if (intent.wifiApState == WifiApManager.WIFI_AP_STATE_FAILED) {
                intent.getIntExtra(WifiApManager.EXTRA_WIFI_AP_FAILURE_REASON, 0)
            } else null
            data.notifyChange()
        }
        private var failureReason: Int? = null
        private var numClients: Int? = null
        private var info = emptyList<Parcelable>()
        private var capability: Parcelable? = null

        init {
            if (Build.VERSION.SDK_INT >= 23) parent.viewLifecycleOwner.lifecycle.addObserver(this)
        }

        override fun onStart(owner: LifecycleOwner) {
            if (Build.VERSION.SDK_INT < 28) {
                parent.requireContext().registerReceiver(receiver,
                    IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))
            } else WifiApCommands.registerSoftApCallback(this)
        }
        override fun onStop(owner: LifecycleOwner) {
            if (Build.VERSION.SDK_INT < 28) {
                parent.requireContext().unregisterReceiver(receiver)
            } else WifiApCommands.unregisterSoftApCallback(this)
        }

        override fun onStateChanged(state: Int, failureReason: Int) {
            if (!WifiApManager.checkWifiApState(state)) return
            this.failureReason = if (state == WifiApManager.WIFI_AP_STATE_FAILED) failureReason else null
            data.notifyChange()
        }
        override fun onNumClientsChanged(numClients: Int) {
            this.numClients = numClients
            data.notifyChange()
        }
        override fun onInfoChanged(info: List<Parcelable>) {
            this.info = info
            data.notifyChange()
        }
        override fun onCapabilityChanged(capability: Parcelable) {
            this.capability = capability
            data.notifyChange()
        }

        override val title get() = parent.getString(R.string.tethering_manage_wifi)
        override val tetherType get() = TetherType.WIFI
        override val type get() = VIEW_TYPE_WIFI

        @TargetApi(30)
        private fun formatCapability(locale: Locale) = capability?.let { parcel ->
            val capability = SoftApCapability(parcel)
            val numClients = numClients
            val maxClients = capability.maxSupportedClients
            var features = capability.supportedFeatures
            if (Build.VERSION.SDK_INT >= 31) for ((flag, band) in arrayOf(
                SoftApCapability.SOFTAP_FEATURE_BAND_24G_SUPPORTED to SoftApConfigurationCompat.BAND_2GHZ,
                SoftApCapability.SOFTAP_FEATURE_BAND_5G_SUPPORTED to SoftApConfigurationCompat.BAND_5GHZ,
                SoftApCapability.SOFTAP_FEATURE_BAND_6G_SUPPORTED to SoftApConfigurationCompat.BAND_6GHZ,
                SoftApCapability.SOFTAP_FEATURE_BAND_60G_SUPPORTED to SoftApConfigurationCompat.BAND_60GHZ,
            )) {
                if (capability.getSupportedChannelList(band).isEmpty()) continue
                // reduce double reporting
                features = features and flag.inv()
            }
            val result = parent.resources.getQuantityText(R.plurals.tethering_manage_wifi_capabilities, numClients ?: 0)
                .format(locale, numClients ?: "?", maxClients, sequence {
                    if (WifiApManager.isApMacRandomizationSupported) yield(parent.getText(
                        R.string.tethering_manage_wifi_feature_ap_mac_randomization))
                    if (Services.wifi.isStaApConcurrencySupported) yield(parent.getText(
                        R.string.tethering_manage_wifi_feature_sta_ap_concurrency))
                    if (Build.VERSION.SDK_INT >= 31) {
                        if (Services.wifi.isBridgedApConcurrencySupported) yield(parent.getText(
                            R.string.tethering_manage_wifi_feature_bridged_ap_concurrency))
                        if (Services.wifi.isStaBridgedApConcurrencySupported) yield(parent.getText(
                            R.string.tethering_manage_wifi_feature_sta_bridged_ap_concurrency))
                    }
                    if (features != 0L) while (features != 0L) {
                        val bit = features.takeLowestOneBit()
                        yield(SoftApCapability.featureLookup(bit, true))
                        features = features and bit.inv()
                    }
                }.joinToSpanned().ifEmpty { parent.getText(R.string.tethering_manage_wifi_no_features) })
            if (Build.VERSION.SDK_INT >= 31) {
                val list = SoftApConfigurationCompat.BAND_TYPES.map { band ->
                    val channels = capability.getSupportedChannelList(band)
                    if (channels.isNotEmpty()) StringBuilder().apply {
                        append(SoftApConfigurationCompat.bandLookup(band, true))
                        append(" (")
                        channels.sort()
                        var pending: Int? = null
                        var last = channels[0]
                        append(last)
                        for (channel in channels.asSequence().drop(1)) {
                            if (channel == last + 1) pending = channel else {
                                pending?.let {
                                    append('-')
                                    append(it)
                                    pending = null
                                }
                                append(',')
                                append(channel)
                            }
                            last = channel
                        }
                        pending?.let {
                            append('-')
                            append(it)
                        }
                        append(')')
                    } else null
                }.filterNotNull()
                if (list.isNotEmpty()) result.append(parent.getText(R.string.tethering_manage_wifi_supported_channels)
                    .format(locale, list.joinToString("; ")))
            }
            result
        } ?: numClients?.let { numClients ->
            app.resources.getQuantityText(R.plurals.tethering_manage_wifi_clients, numClients).format(locale,
                numClients)
        }
        override val text get() = parent.resources.configuration.locale.let { locale ->
            listOfNotNull(failureReason?.let { WifiApManager.failureReasonLookup(it) }, baseError, info.run {
                if (isEmpty()) null else joinToSpanned("\n") @TargetApi(30) { parcel ->
                    val info = SoftApInfo(parcel)
                    val frequency = info.frequency
                    val channel = SoftApConfigurationCompat.frequencyToChannel(frequency)
                    val bandwidth = SoftApInfo.channelWidthLookup(info.bandwidth, true)
                    if (Build.VERSION.SDK_INT >= 31) {
                        val bssid = info.bssid.let { if (it == null) null else makeMacSpan(it.toString()) }
                        val bssidAp = info.apInstanceIdentifier?.let {
                            when (bssid) {
                                null -> it
                                is String -> "$bssid%$it"   // take the fast route if possible
                                else -> SpannableStringBuilder(bssid).append("%$it")
                            }
                        } ?: bssid ?: "?"
                        val timeout = info.autoShutdownTimeoutMillis
                        parent.getText(if (timeout == 0L) {
                            R.string.tethering_manage_wifi_info_timeout_disabled
                        } else R.string.tethering_manage_wifi_info_timeout_enabled).format(locale,
                            frequency, channel, bandwidth, bssidAp, info.wifiStandard,
                            // http://unicode.org/cldr/trac/ticket/3407
                            DateUtils.formatElapsedTime(timeout / 1000))
                    } else parent.getText(R.string.tethering_manage_wifi_info).format(locale,
                        frequency, channel, bandwidth)
                }
            }, formatCapability(locale)).joinToSpanned("\n")
        }

        override fun start() = TetheringManager.startTethering(TetheringManager.TETHERING_WIFI, true, this)
        override fun stop() = TetheringManager.stopTethering(TetheringManager.TETHERING_WIFI, this::onException)
    }
    @RequiresApi(24)
    class Usb(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_usb)
        override val tetherType get() = TetherType.USB
        override val type get() = VIEW_TYPE_USB

        override fun start() = TetheringManager.startTethering(TetheringManager.TETHERING_USB, true, this)
        override fun stop() = TetheringManager.stopTethering(TetheringManager.TETHERING_USB, this::onException)
    }
    @RequiresApi(24)
    class Bluetooth(parent: TetheringFragment, adapter: BluetoothAdapter) :
        TetherManager(parent), DefaultLifecycleObserver {
        private val tethering = BluetoothTethering(parent.requireContext(), adapter) { data.notifyChange() }

        init {
            parent.viewLifecycleOwner.lifecycle.addObserver(this)
        }

        fun ensureInit(context: Context) {
            tethering.ensureInit(context)
            onTetheringStarted()    // force flush
        }
        override fun onResume(owner: LifecycleOwner) {
            if (Build.VERSION.SDK_INT < 31) return
            if (parent.requireContext().checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) ==
                PackageManager.PERMISSION_GRANTED) {
                tethering.ensureInit(parent.requireContext())
            } else parent.requestBluetooth.launch(Manifest.permission.BLUETOOTH_CONNECT)
        }
        override fun onDestroy(owner: LifecycleOwner) = tethering.close()

        override val title get() = parent.getString(R.string.tethering_manage_bluetooth)
        override val tetherType get() = TetherType.BLUETOOTH
        override val type get() = VIEW_TYPE_BLUETOOTH
        override val isStarted get() = tethering.active
        override val text get() = listOfNotNull(
                if (tethering.active == null) tethering.activeFailureCause?.readableMessage else null,
                baseError).joinToString("\n")

        override fun start() = tethering.start(this, parent.requireContext())
        override fun stop() {
            tethering.stop(this::onException)
            onTetheringStarted()    // force flush state
        }
        override fun onClickNull() = ManageBar.start(parent.requireContext())
    }
    @RequiresApi(30)
    class Ethernet(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_ethernet)
        override val tetherType get() = TetherType.ETHERNET
        override val type get() = VIEW_TYPE_ETHERNET

        override fun start() = TetheringManager.startTethering(TetheringManager.TETHERING_ETHERNET, true, this)
        override fun stop() = TetheringManager.stopTethering(TetheringManager.TETHERING_ETHERNET, this::onException)
    }
    @RequiresApi(30)
    class Ncm(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_ncm)
        override val tetherType get() = TetherType.NCM
        override val type get() = VIEW_TYPE_NCM

        override fun start() = TetheringManager.startTethering(TetheringManager.TETHERING_NCM, true, this)
        override fun stop() = TetheringManager.stopTethering(TetheringManager.TETHERING_NCM, this::onException)
    }

    @Suppress("DEPRECATION")
    @Deprecated("Not usable since API 26, malfunctioning on API 25")
    class WifiLegacy(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_wifi_legacy)
        override val tetherType get() = TetherType.WIFI
        override val type get() = VIEW_TYPE_WIFI_LEGACY

        override fun start() = try {
            WifiApManager.start()
        } catch (e: Exception) {
            onException(e)
        }
        override fun stop() = try {
            WifiApManager.stop()
        } catch (e: Exception) {
            onException(e)
        }
    }
}
