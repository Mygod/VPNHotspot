package be.mygod.vpnhotspot.util

object RangeInput {
    fun toString(input: IntArray) = StringBuilder().apply {
        if (input.isEmpty()) return@apply
        input.sort()
        var pending: Int? = null
        var last = input[0]
        append(last)
        for (channel in input.asSequence().drop(1)) {
            if (channel == last + 1) pending = channel else {
                pending?.let {
                    append('-')
                    append(it)
                    pending = null
                }
                append(",\u200b")   // zero-width space to save space
                append(channel)
            }
            last = channel
        }
        pending?.let {
            append('-')
            append(it)
        }
    }.toString()
    fun toString(input: Set<Int>?) = input?.run { toString(toIntArray()) }

    fun fromString(input: CharSequence?, min: Int = 1, max: Int = 999) = mutableSetOf<Int>().apply {
        if (input == null) return@apply
        for (unit in input.split(',')) {
            if (unit.isBlank()) continue
            val blocks = unit.split('-', limit = 2).map { i ->
                i.trim { it == '\u200b' || it.isWhitespace() }.toInt()
            }
            require(blocks[0] in min..max) { "Out of range: ${blocks[0]}" }
            if (blocks.size == 2) {
                require(blocks[1] in min..max) { "Out of range: ${blocks[1]}" }
                addAll(blocks[0]..blocks[1])
            } else add(blocks[0])
        }
    }
}
