package be.mygod.vpnhotspot.util

import android.text.Spannable
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.SpannedString
import java.util.*

/**
 * Provides [String.format] style functions that work with [Spanned] strings and preserve formatting.
 *
 * https://github.com/george-steel/android-utils/blob/289aff11e53593a55d780f9f5986e49343a79e55/src/org/oshkimaadziig/george/androidutils/SpanFormatter.java
 *
 * @author George T. Steel
 */
object SpanFormatter {
    private val formatSequence = "%([0-9]+\\$|<?)([^a-zA-z%]*)([[a-zA-Z%]&&[^tT]]|[tT][a-zA-Z])".toPattern()

    /**
     * Version of [String.format] that works on [Spanned] strings to preserve rich text formatting.
     * Both the `format` as well as any `%s args` can be Spanned and will have their formatting preserved.
     * Due to the way [Spannable]s work, any argument's spans will can only be included **once** in the result.
     * Any duplicates will appear as text only.
     *
     * @param format the format string (see [java.util.Formatter.format])
     * @param args
     * the list of arguments passed to the formatter. If there are
     * more arguments than required by `format`,
     * additional arguments are ignored.
     * @return the formatted string (with spans).
     */
    fun format(format: CharSequence, vararg args: Any) = format(Locale.getDefault(), format, *args)

    /**
     * Version of [String.format] that works on [Spanned] strings to preserve rich text formatting.
     * Both the `format` as well as any `%s args` can be Spanned and will have their formatting preserved.
     * Due to the way [Spannable]s work, any argument's spans will can only be included **once** in the result.
     * Any duplicates will appear as text only.
     *
     * @param locale
     * the locale to apply; `null` value means no localization.
     * @param format the format string (see [java.util.Formatter.format])
     * @param args
     * the list of arguments passed to the formatter.
     * @return the formatted string (with spans).
     * @see String.format
     */
    fun format(locale: Locale, format: CharSequence, vararg args: Any): SpannedString {
        val out = SpannableStringBuilder(format)

        var i = 0
        var argAt = -1

        while (i < out.length) {
            val m = formatSequence.matcher(out)
            if (!m.find(i)) break
            i = m.start()
            val exprEnd = m.end()

            val argTerm = m.group(1)!!
            val modTerm = m.group(2)
            val typeTerm = m.group(3)

            val cookedArg = when (typeTerm) {
                "%" -> "%"
                "n" -> "\n"
                else -> {
                    val argItem = args[when (argTerm) {
                        "" -> ++argAt
                        "<" -> argAt
                        else -> Integer.parseInt(argTerm.substring(0, argTerm.length - 1)) - 1
                    }]
                    if (typeTerm == "s" && argItem is Spanned) argItem else {
                        String.format(locale, "%$modTerm$typeTerm", argItem)
                    }
                }
            }

            out.replace(i, exprEnd, cookedArg)
            i += cookedArg.length
        }

        return SpannedString(out)
    }
}
