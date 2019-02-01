package be.mygod.vpnhotspot

import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.view.ViewStub
import android.widget.Button
import android.widget.LinearLayout
import androidx.appcompat.app.AppCompatDialogFragment
import be.mygod.vpnhotspot.util.launchUrl

/**
 * Based on: https://github.com/PrivacyApps/donations/blob/747d36a18433c7e9329691054122a8ad337a62d2/Donations/src/main/java/org/sufficientlysecure/donations/DonationsFragment.java
 */
class EBegFragment : AppCompatDialogFragment() {
    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View =
            inflater.inflate(R.layout.fragment_ebeg, container, false)

    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        super.onViewCreated(view, savedInstanceState)
        view.findViewById<LinearLayout>(R.id.donations__google).visibility = View.GONE
        (view.findViewById<ViewStub>(R.id.donations__more_stub).inflate() as Button).setOnClickListener {
            requireContext().launchUrl("https://mygod.be/donate/")
        }
    }
}
