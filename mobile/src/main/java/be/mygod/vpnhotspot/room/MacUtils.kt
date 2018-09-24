package be.mygod.vpnhotspot.room

import java.nio.ByteBuffer
import java.nio.ByteOrder

fun String.macToLong(): Long = ByteBuffer.allocate(8).run {
    order(ByteOrder.LITTLE_ENDIAN)
    mark()
    put(split(':').map { Integer.parseInt(it, 16).toByte() }.toByteArray())
    reset()
    long
}

fun Long.macToString(): String = ByteBuffer.allocate(8).run {
    order(ByteOrder.LITTLE_ENDIAN)
    putLong(this@macToString)
    array().take(6).joinToString(":") { "%02x".format(it) }
}
