package be.mygod.vpnhotspot

import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.ArrayAdapter
import androidx.appcompat.app.AppCompatDialogFragment
import androidx.lifecycle.lifecycleScope
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.databinding.FragmentEbegBinding
import be.mygod.vpnhotspot.util.launchUrl
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.android.billingclient.api.BillingClient
import com.android.billingclient.api.BillingClientStateListener
import com.android.billingclient.api.BillingFlowParams
import com.android.billingclient.api.BillingResult
import com.android.billingclient.api.ConsumeParams
import com.android.billingclient.api.ProductDetails
import com.android.billingclient.api.Purchase
import com.android.billingclient.api.PurchasesUpdatedListener
import com.android.billingclient.api.QueryProductDetailsParams
import com.android.billingclient.api.QueryPurchasesParams
import com.android.billingclient.api.consumePurchase
import com.android.billingclient.api.queryProductDetails
import com.android.billingclient.api.queryPurchasesAsync
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber

/**
 * Based on: https://github.com/PrivacyApps/donations/blob/747d36a18433c7e9329691054122a8ad337a62d2/Donations/src/main/java/org/sufficientlysecure/donations/DonationsFragment.java
 */
class EBegFragment : AppCompatDialogFragment() {
    companion object : BillingClientStateListener, PurchasesUpdatedListener {
        private val billingClient by lazy {
            BillingClient.newBuilder(app).apply {
                enablePendingPurchases()
            }.setListener(this).build()
        }
        private var instance: EBegFragment? = null

        override fun onBillingSetupFinished(billingResult: BillingResult) {
            if (billingResult.responseCode != BillingClient.BillingResponseCode.OK) {
                Timber.e("onBillingSetupFinished: ${billingResult.responseCode}")
            } else {
                instance?.onBillingConnected()
                GlobalScope.launch(Dispatchers.Main.immediate) {
                    val result = billingClient.queryPurchasesAsync(QueryPurchasesParams.newBuilder().apply {
                        setProductType(BillingClient.ProductType.INAPP)
                    }.build())
                    onPurchasesUpdated(result.billingResult, result.purchasesList)
                }
            }
        }

        override fun onBillingServiceDisconnected() {
            Timber.e("onBillingServiceDisconnected")
            if (instance != null) billingClient.startConnection(this)
        }

        override fun onPurchasesUpdated(billingResult: BillingResult, purchases: List<Purchase>?) {
            if (billingResult.responseCode == BillingClient.BillingResponseCode.OK && purchases != null) {
                // directly consume in-app purchase, so that people can donate multiple times
                purchases.filter { it.purchaseState == Purchase.PurchaseState.PURCHASED }.map(this::consumePurchase)
            } else Timber.e("onPurchasesUpdated: ${billingResult.responseCode}")
        }

        private fun consumePurchase(purchase: Purchase) = GlobalScope.launch(Dispatchers.Main.immediate) {
            billingClient.consumePurchase(ConsumeParams.newBuilder().apply {
                setPurchaseToken(purchase.purchaseToken)
            }.build()).apply {
                if (billingResult.responseCode == BillingClient.BillingResponseCode.OK) {
                    SmartSnackbar.make(R.string.donations__thanks_dialog).show()
                } else Timber.e("onConsumeResponse: ${billingResult.responseCode}")
            }
        }
    }

    private lateinit var binding: FragmentEbegBinding
    private var productDetails: List<ProductDetails>? = null
        set(value) {
            field = value
            binding.donationsGoogleAndroidMarketSpinner.apply {
                val adapter = ArrayAdapter(context ?: return, android.R.layout.simple_spinner_item,
                        value?.map { it.oneTimePurchaseOfferDetails?.formattedPrice } ?: listOf("…"))
                adapter.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
                setAdapter(adapter)
            }
        }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View {
        binding = FragmentEbegBinding.inflate(inflater, container, false)
        return binding.root
    }
    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        super.onViewCreated(view, savedInstanceState)
        dialog!!.setTitle(R.string.settings_misc_donate)
        binding.donationsGoogleAndroidMarketDonateButton.setOnClickListener {
            val product = productDetails?.getOrNull(binding.donationsGoogleAndroidMarketSpinner.selectedItemPosition)
            if (product != null) billingClient.launchBillingFlow(requireActivity(), BillingFlowParams.newBuilder().apply {
                setProductDetailsParamsList(listOf(BillingFlowParams.ProductDetailsParams.newBuilder().apply {
                    setProductDetails(product)
                }.build()))
            }.build()) else SmartSnackbar.make(R.string.donations__google_android_market_not_supported).show()
        }
        binding.donationsMoreDonateButton.setOnClickListener {
            requireContext().launchUrl("https://mygod.be/donate/")
        }
    }

    override fun onStart() {
        super.onStart()
        instance = this
        billingClient.startConnection(EBegFragment)
    }

    private fun onBillingConnected() = viewLifecycleOwner.lifecycleScope.launch {
        billingClient.queryProductDetails(QueryProductDetailsParams.newBuilder().apply {
            setProductList(listOf(
                "donate001", "donate002", "donate005", "donate010", "donate020", "donate050",
                "donate100", "donate200", "donatemax",
            ).map {
                QueryProductDetailsParams.Product.newBuilder().apply {
                    setProductId(it)
                    setProductType(BillingClient.ProductType.INAPP)
                }.build()
            })
        }.build()).apply {
            if (billingResult.responseCode != BillingClient.BillingResponseCode.OK) {
                Timber.e("queryProductDetails: ${billingResult.responseCode}")
                SmartSnackbar.make(R.string.donations__google_android_market_not_supported).show()
            } else productDetails = productDetailsList
        }
    }

    override fun onStop() {
        instance = null
        super.onStop()
    }
}
