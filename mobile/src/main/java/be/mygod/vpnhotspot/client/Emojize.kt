package be.mygod.vpnhotspot.client

import androidx.emoji.text.EmojiCompat

fun emojize(text: CharSequence?): CharSequence? = if (text == null) null else try {
    EmojiCompat.get().process(text)
} catch (_: IllegalStateException) {
    text
}
