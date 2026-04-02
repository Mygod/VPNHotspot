package be.mygod.vpnhotspot

import androidx.appcompat.app.AppCompatDelegate
import androidx.core.content.edit
import androidx.core.os.LocaleListCompat
import be.mygod.vpnhotspot.App.Companion.app

object AppLocaleManager {
    const val KEY = "ui.language"
    const val LANGUAGE_SYSTEM = ""
    const val LANGUAGE_ENGLISH = "en"
    const val LANGUAGE_SIMPLIFIED_CHINESE = "zh-CN"

    private fun toLocaleList(languageTag: String) = LocaleListCompat.forLanguageTags(languageTag.trim())
    private fun storedLanguageTag() = if (app.pref.contains(KEY)) app.pref.getString(KEY, LANGUAGE_SYSTEM) else null

    fun currentLanguageTag() = storedLanguageTag().orEmpty().ifEmpty {
        when (AppCompatDelegate.getApplicationLocales().toLanguageTags()) {
            LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE -> AppCompatDelegate.getApplicationLocales().toLanguageTags()
            else -> LANGUAGE_SYSTEM
        }
    }

    fun applyPersistedLanguage() {
        storedLanguageTag()?.let(::applyLanguage)
    }

    fun applyLanguage(languageTag: String) {
        AppCompatDelegate.setApplicationLocales(toLocaleList(languageTag))
    }

    fun updateLanguage(languageTag: String) {
        val normalized = languageTag.trim()
        app.pref.edit { putString(KEY, normalized) }
        applyLanguage(normalized)
    }
}
