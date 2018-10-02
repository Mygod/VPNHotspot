package be.mygod.vpnhotspot

import timber.log.Timber

fun initTimber() = Timber.plant(Timber.DebugTree())

fun debugLog(tag: String?, message: String?) = Timber.d(tag, message)
