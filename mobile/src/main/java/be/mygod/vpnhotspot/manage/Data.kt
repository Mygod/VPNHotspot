package be.mygod.vpnhotspot.manage

import android.databinding.BaseObservable

abstract class Data : BaseObservable() {
    abstract val icon: Int
    abstract val title: CharSequence
    abstract val text: CharSequence
    abstract val active: Boolean
    abstract val selectable: Boolean
}
