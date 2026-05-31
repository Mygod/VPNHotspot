use std::ffi::CStr;
use std::io;

pub(crate) fn android_api_level() -> i32 {
    extern "C" {
        fn android_get_device_api_level() -> libc::c_int;
    }
    unsafe { android_get_device_api_level() as i32 }
}

pub(crate) fn kernel_release() -> io::Result<String> {
    let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
    if unsafe { libc::uname(uts.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let uts = unsafe { uts.assume_init() };
    let release = unsafe { CStr::from_ptr(uts.release.as_ptr()) };
    Ok(release.to_string_lossy().into_owned())
}
