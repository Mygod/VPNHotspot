package be.mygod.vpnhotspot

import timber.log.Timber

fun initTimber() = Timber.plant(Timber.DebugTree())

fun debugLog(tag: String?, message: String?) = Timber.tag(tag).d(message)

fun timberSetString(key: String, value: String?) = Timber.tag(key).d(value)
