package be.mygod.vpnhotspot

import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.Spinner
import androidx.appcompat.app.AppCompatDialogFragment
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.launchUrl
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.android.billingclient.api.*
import kotlinx.android.synthetic.main.fragment_ebeg.view.*
import timber.log.Timber

/**
 * Based on: https://github.com/PrivacyApps/donations/blob/747d36a18433c7e9329691054122a8ad337a62d2/Donations/src/main/java/org/sufficientlysecure/donations/DonationsFragment.java
 */
class EBegFragment : AppCompatDialogFragment(), SkuDetailsResponseListener {
    companion object : BillingClientStateListener, PurchasesUpdatedListener, ConsumeResponseListener {
        private lateinit var billingClient: BillingClient

        fun init() {
            billingClient = BillingClient.newBuilder(app).apply {
                enablePendingPurchases()
            }.setListener(this).build().also { it.startConnection(this) }
        }

        override fun onBillingSetupFinished(billingResult: BillingResult?) {
            if (billingResult?.responseCode == BillingClient.BillingResponseCode.OK) {
                billingClient.queryPurchases(BillingClient.SkuType.INAPP).apply {
                    if (responseCode == BillingClient.BillingResponseCode.OK) {
                        onPurchasesUpdated(this.billingResult, purchasesList)
                    }
                }
            } else Timber.e("onBillingSetupFinished: ${billingResult?.responseCode}")
        }

        override fun onBillingServiceDisconnected() {
            Timber.e("onBillingServiceDisconnected")
            billingClient.startConnection(this)
        }

        override fun onPurchasesUpdated(billingResult: BillingResult?, purchases: MutableList<Purchase>?) {
            if (billingResult?.responseCode == BillingClient.BillingResponseCode.OK && purchases != null) {
                // directly consume in-app purchase, so that people can donate multiple times
                for (purchase in purchases) if (purchase.purchaseState == Purchase.PurchaseState.PURCHASED) {
                    billingClient.consumeAsync(ConsumeParams.newBuilder().apply {
                        setPurchaseToken(purchase.purchaseToken)
                    }.build(), this)
                }
            } else Timber.e("onPurchasesUpdated: ${billingResult?.responseCode}")
        }

        override fun onConsumeResponse(billingResult: BillingResult?, purchaseToken: String?) {
            if (billingResult?.responseCode == BillingClient.BillingResponseCode.OK) {
                SmartSnackbar.make(R.string.donations__thanks_dialog).show()
            } else Timber.e("onConsumeResponse: ${billingResult?.responseCode}")
        }
    }

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
        googleSpinner = view.donations__google_android_market_spinner
        billingClient.querySkuDetailsAsync(
                SkuDetailsParams.newBuilder().apply {
                    setSkusList(listOf("donate001", "donate002", "donate005", "donate010", "donate020", "donate050",
                            "donate100", "donate200", "donatemax"))
                    setType(BillingClient.SkuType.INAPP)
                }.build(), this)
        view.donations__google_android_market_donate_button.setOnClickListener {
            val sku = skus?.getOrNull(googleSpinner.selectedItemPosition)
            if (sku != null) billingClient.launchBillingFlow(requireActivity(), BillingFlowParams.newBuilder().apply {
                setSkuDetails(sku)
            }.build()) else SmartSnackbar.make(R.string.donations__google_android_market_not_supported).show()
        }
        @Suppress("ConstantConditionIf")
        if (BuildConfig.DONATIONS) (view.donations__more_stub.inflate() as Button)
                .setOnClickListener { requireContext().launchUrl("https://mygod.be/donate/") }
    }

    override fun onSkuDetailsResponse(billingResult: BillingResult?, skuDetailsList: MutableList<SkuDetails>?) {
        if (billingResult?.responseCode == BillingClient.BillingResponseCode.OK) skus = skuDetailsList else {
            Timber.e("onSkuDetailsResponse: ${billingResult?.responseCode}")
            SmartSnackbar.make(R.string.donations__google_android_market_not_supported).show()
        }
    }
}
