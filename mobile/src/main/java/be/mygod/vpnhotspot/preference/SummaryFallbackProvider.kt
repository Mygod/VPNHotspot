package be.mygod.vpnhotspot.preference

import androidx.preference.EditTextPreference
import androidx.preference.Preference

class SummaryFallbackProvider(preference: Preference) : Preference.SummaryProvider<EditTextPreference> {
    val fallback = preference.summary
    init {
        preference.summaryProvider = this
    }

    override fun provideSummary(preference: EditTextPreference) = preference.text.let {
        if (it.isNullOrEmpty()) fallback else it
    }
}
