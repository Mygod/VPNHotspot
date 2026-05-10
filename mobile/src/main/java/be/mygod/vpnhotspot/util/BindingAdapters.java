package be.mygod.vpnhotspot.util;

import android.view.View;
import android.widget.ImageView;
import androidx.annotation.DrawableRes;
import androidx.databinding.BindingAdapter;

public final class BindingAdapters {
    private BindingAdapters() { }

    @BindingAdapter("android:src")
    public static void setImageResource(ImageView imageView, @DrawableRes int resource) {
        imageView.setImageResource(resource);
    }

    @BindingAdapter("android:visibility")
    public static void setVisibility(View view, boolean value) {
        view.setVisibility(value ? View.VISIBLE : View.GONE);
    }
}
