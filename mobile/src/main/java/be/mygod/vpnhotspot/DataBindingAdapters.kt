package be.mygod.vpnhotspot

import android.databinding.BindingAdapter
import android.support.annotation.DrawableRes
import android.widget.ImageView

object DataBindingAdapters {
    @JvmStatic
    @BindingAdapter("android:src")
    fun setImageResource(imageView: ImageView, @DrawableRes resource: Int) = imageView.setImageResource(resource)
}
