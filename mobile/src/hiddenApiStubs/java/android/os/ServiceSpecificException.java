package android.os;

/**
 * Compile-only system API used to preserve Binder service status in per-event Crashlytics keys.
 *
 * <p>Descriptor: {@code Landroid/os/ServiceSpecificException;->errorCode:I,sdk,system-api,test-api}</p>
 *
 * <p>Sources:
 * https://android.googlesource.com/platform/frameworks/base/+/android-10.0.0_r1/core/java/android/os/ServiceSpecificException.java#35
 * https://android.googlesource.com/platform/frameworks/base/+/1cdfff555f4a21f71ccc978290e2e212e2f8b168/core/java/android/os/ServiceSpecificException.java#38
 * </p>
 */
public class ServiceSpecificException extends RuntimeException {
    public final int errorCode;

    public ServiceSpecificException(int errorCode) {
        this.errorCode = errorCode;
    }
}
