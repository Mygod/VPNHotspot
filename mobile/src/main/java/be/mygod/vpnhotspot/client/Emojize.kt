package be.mygod.vpnhotspot.client

import androidx.emoji.text.EmojiCompat
import java.lang.IllegalStateException

fun emojize(text: CharSequence?): CharSequence? = if (text == null) null else try {
    EmojiCompat.get().process(text)
} catch (_: IllegalStateException) {
    text
}
