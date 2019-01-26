package be.mygod.vpnhotspot

import android.content.DialogInterface
import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.view.ViewStub
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.Spinner
import androidx.annotation.StringRes
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatDialogFragment
import androidx.versionedparcelable.VersionedParcelable
import be.mygod.vpnhotspot.util.launchUrl
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.android.billingclient.api.*
import timber.log.Timber

/**
 * Based on: https://github.com/PrivacyApps/donations/blob/747d36a18433c7e9329691054122a8ad337a62d2/Donations/src/main/java/org/sufficientlysecure/donations/DonationsFragment.java
 */
class EBegFragment : AppCompatDialogFragment(), PurchasesUpdatedListener, BillingClientStateListener,
        SkuDetailsResponseListener, ConsumeResponseListener {
    data class MessageArg(@StringRes val title: Int, @StringRes val message: Int) : VersionedParcelable
    class MessageDialogFragment : AlertDialogFragment<MessageArg, Empty>() {
        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setTitle(arg.title)
            setMessage(arg.message)
            setNeutralButton(R.string.donations__button_close, null)
        }
    }

    private lateinit var billingClient: BillingClient
    private lateinit var googleSpinner: Spinner
    private var skus: MutableList<SkuDetails>? = null
        set(value) {
            field = value
            googleSpinner.apply {
                val adapter = ArrayAdapter(context ?: return, android.R.layout.simple_spinner_item,
                        value?.map { it.price } ?: listOf("…"))
                adapter.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
                setAdapter(adapter)
            }
        }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View =
            inflater.inflate(R.layout.fragment_ebeg, container, false)
    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        super.onViewCreated(view, savedInstanceState)
        dialog!!.setTitle(R.string.settings_misc_donate)
        googleSpinner = view.findViewById(R.id.donations__google_android_market_spinner)
        onBillingServiceDisconnected()
        view.findViewById<Button>(R.id.donations__google_android_market_donate_button).setOnClickListener {
            val sku = skus?.getOrNull(googleSpinner.selectedItemPosition)
            if (sku == null) {
                openDialog(R.string.donations__google_android_market_not_supported_title,
                        R.string.donations__google_android_market_not_supported)
            } else billingClient.launchBillingFlow(requireActivity(), BillingFlowParams.newBuilder()
                    .setSkuDetails(sku).build())
        }
        @Suppress("ConstantConditionIf")
        if (BuildConfig.DONATIONS) (view.findViewById<ViewStub>(R.id.donations__more_stub).inflate() as Button)
                .setOnClickListener { requireContext().launchUrl("https://mygod.be/donate/") }
    }

    private fun openDialog(@StringRes title: Int, @StringRes message: Int) {
        val fragmentManager = fragmentManager
        if (fragmentManager == null) SmartSnackbar.make(message).show() else try {
            MessageDialogFragment().withArg(MessageArg(title, message)).show(fragmentManager, "MessageDialogFragment")
        } catch (e: IllegalStateException) {
            SmartSnackbar.make(message).show()
        }
    }

    override fun onBillingServiceDisconnected() {
        skus = null
        billingClient = BillingClient.newBuilder(context ?: return).setListener(this).build()
                .also { it.startConnection(this) }
    }

    override fun onBillingSetupFinished(responseCode: Int) {
        if (responseCode == BillingClient.BillingResponse.OK) {
            billingClient.querySkuDetailsAsync(
                    SkuDetailsParams.newBuilder().apply {
                        setSkusList(listOf("donate001", "donate002", "donate005", "donate010", "donate020", "donate050",
                                "donate100", "donate200", "donatemax"))
                        setType(BillingClient.SkuType.INAPP)
                    }.build(), this)
        } else Timber.e("onBillingSetupFinished: $responseCode")
    }

    override fun onSkuDetailsResponse(responseCode: Int, skuDetailsList: MutableList<SkuDetails>?) {
        if (responseCode == BillingClient.BillingResponse.OK) skus = skuDetailsList
        else Timber.e("onSkuDetailsResponse: $responseCode")
    }

    override fun onPurchasesUpdated(responseCode: Int, purchases: MutableList<Purchase>?) {
        if (responseCode == BillingClient.BillingResponse.OK && purchases != null) {
            // directly consume in-app purchase, so that people can donate multiple times
            purchases.forEach { billingClient.consumeAsync(it.purchaseToken, this) }
        } else Timber.e("onPurchasesUpdated: $responseCode")
    }

    override fun onConsumeResponse(responseCode: Int, purchaseToken: String?) {
        if (responseCode == BillingClient.BillingResponse.OK) {
            openDialog(R.string.donations__thanks_dialog_title, R.string.donations__thanks_dialog)
            dismissAllowingStateLoss()
        } else Timber.e("onConsumeResponse: $responseCode")
    }
}
