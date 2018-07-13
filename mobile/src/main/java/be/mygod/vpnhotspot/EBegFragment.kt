package be.mygod.vpnhotspot

import android.app.AlertDialog
import android.net.Uri
import android.os.Bundle
import android.support.v4.app.DialogFragment
import android.util.Log
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.view.ViewStub
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.Spinner
import com.android.billingclient.api.*
import com.crashlytics.android.Crashlytics

/**
 * Based on: https://github.com/PrivacyApps/donations/blob/747d36a18433c7e9329691054122a8ad337a62d2/Donations/src/main/java/org/sufficientlysecure/donations/DonationsFragment.java
 */
class EBegFragment : DialogFragment(), PurchasesUpdatedListener, BillingClientStateListener,
        SkuDetailsResponseListener, ConsumeResponseListener {
    companion object {
        private const val TAG = "EBegFragment"
    }

    private lateinit var billingClient: BillingClient
    private lateinit var googleSpinner: Spinner
    private var skus: MutableList<SkuDetails>? = null
        set(value) {
            field = value
            googleSpinner.apply {
                val adapter = ArrayAdapter(requireContext(), android.R.layout.simple_spinner_item,
                        value?.map { it.price } ?: emptyList())
                adapter.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
                setAdapter(adapter)
            }
        }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View =
            inflater.inflate(R.layout.fragment_ebeg, container, false)
    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        super.onViewCreated(view, savedInstanceState)
        googleSpinner = view.findViewById(R.id.donations__google_android_market_spinner)
        billingClient = BillingClient.newBuilder(view.context).setListener(this).build()
        onBillingServiceDisconnected()
        view.findViewById<Button>(R.id.donations__google_android_market_donate_button).setOnClickListener {
            val skus = skus
            if (skus == null) {
                openDialog(android.R.drawable.ic_dialog_alert,
                        R.string.donations__google_android_market_not_supported_title,
                        getString(R.string.donations__google_android_market_not_supported))
            } else billingClient.launchBillingFlow(requireActivity(), BillingFlowParams.newBuilder()
                    .setSku(skus[googleSpinner.selectedItemPosition].sku).setType(BillingClient.SkuType.INAPP).build())
        }
        @Suppress("ConstantConditionIf")
        if (BuildConfig.DONATIONS) (view.findViewById<ViewStub>(R.id.donations__more_stub).inflate() as Button)
                .setOnClickListener {
                    (activity as MainActivity).launchUrl(Uri.parse("https://mygod.be/donate/"))
                }
    }

    private fun openDialog(icon: Int, title: Int, message: String) = AlertDialog.Builder(requireContext()).apply {
        setIcon(icon)
        setTitle(title)
        setMessage(message)
        isCancelable = true
        setNeutralButton(R.string.donations__button_close) { dialog, _ -> dialog.dismiss() }
    }.show()

    override fun onBillingServiceDisconnected() {
        skus = null
        billingClient.startConnection(this)
    }
    override fun onBillingSetupFinished(responseCode: Int) {
        if (responseCode == BillingClient.BillingResponse.OK) {
            billingClient.querySkuDetailsAsync(
                    SkuDetailsParams.newBuilder().apply {
                        setSkusList(listOf("donate001", "donate002", "donate005", "donate010", "donate020", "donate050",
                                "donate100", "donate200", "donatemax"))
                        setType(BillingClient.SkuType.INAPP)
                    }.build(), this)
        } else Crashlytics.log(Log.ERROR, TAG, "onBillingSetupFinished: $responseCode")
    }

    override fun onSkuDetailsResponse(responseCode: Int, skuDetailsList: MutableList<SkuDetails>?) {
        if (responseCode == BillingClient.BillingResponse.OK) skus = skuDetailsList
        else Crashlytics.log(Log.ERROR, TAG, "onSkuDetailsResponse: $responseCode")
    }

    override fun onPurchasesUpdated(responseCode: Int, purchases: MutableList<Purchase>?) {
        if (responseCode == BillingClient.BillingResponse.OK && purchases != null) {
            // directly consume in-app purchase, so that people can donate multiple times
            purchases.forEach { billingClient.consumeAsync(it.purchaseToken, this) }
        } else Crashlytics.log(Log.ERROR, TAG, "onPurchasesUpdated: $responseCode")
    }
    override fun onConsumeResponse(responseCode: Int, purchaseToken: String?) {
        if (responseCode == BillingClient.BillingResponse.OK) {
            openDialog(android.R.drawable.ic_dialog_info, R.string.donations__thanks_dialog_title,
                    getString(R.string.donations__thanks_dialog))
        } else Crashlytics.log(Log.ERROR, TAG, "onConsumeResponse: $responseCode")
    }
}
