package be.mygod.vpnhotspot.util

import java.util.concurrent.ConcurrentHashMap

/**
 * These class are based off https://github.com/1blustone/kotlin-events.
 */
open class Event0 : ConcurrentHashMap<Any, () -> Unit>() {
    operator fun invoke() {
        for ((_, handler) in this) handler()
    }
}

class StickyEvent0 : Event0() {
    override fun put(key: Any, value: () -> Unit): (() -> Unit)? =
            super.put(key, value).also { if (it == null) value() }
}

open class Event1<T> : ConcurrentHashMap<Any, (T) -> Unit>() {
    operator fun invoke(arg: T) {
        for ((_, handler) in this) handler(arg)
    }
}

class StickyEvent1<T>(private val fire: () -> T) : Event1<T>() {
    override fun put(key: Any, value: (T) -> Unit): ((T) -> Unit)? =
            super.put(key, value).also { if (it == null) value(fire()) }
}

open class Event2<T1, T2> : ConcurrentHashMap<Any, (T1, T2) -> Unit>() {
    operator fun invoke(arg1: T1, arg2: T2) {
        for ((_, handler) in this) handler(arg1, arg2)
    }
}
