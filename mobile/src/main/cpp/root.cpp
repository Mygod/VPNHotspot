#include <cerrno>
#include <fcntl.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/bpf.h>
#include <linux/unistd.h>
#include <jni.h>
#include <android/log.h>
#include <vector>

#define LOG_TAG "JniRoot"
#define ALOG(priority, tag, fmt, ...) \
    __android_log_print(ANDROID_##priority, tag, fmt, __VA_ARGS__)
#define ALOGE(...) ((void)ALOG(LOG_ERROR, LOG_TAG, __VA_ARGS__))

static int ThrowException(JNIEnv* env, const char* className, const char* ctorSig, ...) {
    jclass exceptionClass = env->FindClass(className);
    if (exceptionClass == nullptr) {
        ALOGE("Unable to find exception class %s", className);
        /* an exception, most likely ClassNotFoundException, will now be pending */
        return -1;
    }
    int status = -1;
    jmethodID init = env->GetMethodID(exceptionClass, "<init>", ctorSig);
    if (init != nullptr) {
        va_list args;
        va_start(args, ctorSig);
        jobject instance = env->NewObjectV(exceptionClass, init, args);
        va_end(args);
        if (instance == nullptr) {
            ALOGE("Failed to construct '%s'", className);
        } else if (env->Throw((jthrowable) instance) != JNI_OK) {
            ALOGE("Failed to throw '%s'", className);
            /* an exception, most likely OOM, will now be pending */
        } else status = 0;
    } else ALOGE("Failed to find constructor for '%s' '%s'", className, ctorSig);
    env->DeleteLocalRef(exceptionClass);
    return status;
}

#define THROW_EXCEPTION_WITH_MESSAGE(env, className, ctorSig, msg, ...) ({                 \
    jstring _detailMessage = env->NewStringUTF(msg);                                 \
    int _status = ThrowException(env, className, ctorSig, _detailMessage, ## __VA_ARGS__); \
    if (_detailMessage != NULL) {                                                          \
        env->DeleteLocalRef(_detailMessage);                                       \
    }                                                                                      \
    _status; })

int jniThrowErrnoException(JNIEnv* env, const char* functionName, int errno_value) {
    return THROW_EXCEPTION_WITH_MESSAGE(env, "android/system/ErrnoException",
                                        "(Ljava/lang/String;I)V", functionName, errno_value);
}

int jniThrowIllegalArgumentException(JNIEnv* env, const char* message) {
    return THROW_EXCEPTION_WITH_MESSAGE(env, "java/lang/IllegalArgumentException",
                                        "(Ljava/lang/String;)V", message);
}

static void writeErrnoAndExit(int fd, int err) {
    while (write(fd, &err, sizeof(err)) < 0 && errno == EINTR) { }
    _exit(127);
}

extern "C" JNIEXPORT void JNICALL
Java_be_mygod_vpnhotspot_root_Jni_launchProcess(JNIEnv *env, [[maybe_unused]] jobject obj,
                                                jobjectArray command, jint stdinFd, jint stdoutFd,
                                                jint stderrFd) {
    const auto argc = command == nullptr ? 0 : env->GetArrayLength(command);
    if (argc <= 0) {
        jniThrowIllegalArgumentException(env, "Empty command");
        return;
    }
    std::vector<jstring> strings(argc);
    std::vector<char *> argv(argc + 1);
    [&]() {
        for (jsize i = 0; i < argc; ++i) {
            strings[i] = static_cast<jstring>(env->GetObjectArrayElement(command, i));
            if (strings[i] == nullptr) {
                jniThrowIllegalArgumentException(env, "Null command element");
                return;
            }
            argv[i] = const_cast<char *>(env->GetStringUTFChars(strings[i], nullptr));
            if (argv[i] == nullptr) return;
        }
        argv[argc] = nullptr;

        int failPipe[2] = {-1, -1};
        if (pipe2(failPipe, O_CLOEXEC) < 0) {
            jniThrowErrnoException(env, "pipe2", errno);
            return;
        }
        pid_t pid = fork();
        if (pid < 0) {
            jniThrowErrnoException(env, "fork", errno);
            close(failPipe[0]);
            close(failPipe[1]);
            return;
        }
        if (pid == 0) {
            close(failPipe[0]);
            pid = fork();
            if (pid < 0) writeErrnoAndExit(failPipe[1], errno);
            if (pid > 0) _exit(0);
            if (dup2(stdinFd, STDIN_FILENO) < 0 || dup2(stdoutFd, STDOUT_FILENO) < 0 ||
                    dup2(stderrFd, STDERR_FILENO) < 0) {
                writeErrnoAndExit(failPipe[1], errno);
            }
            long maxFd = sysconf(_SC_OPEN_MAX);
            if (maxFd < 0 || maxFd > 65536) maxFd = 65536;
            for (int fd = 3; fd < maxFd; ++fd) if (fd != failPipe[1]) close(fd);
            execv(argv[0], argv.data());
            writeErrnoAndExit(failPipe[1], errno);
        }

        close(failPipe[1]);
        int err = 0;
        ssize_t bytes = 0;
        do {
            bytes = read(failPipe[0], &err, sizeof(err));
        } while (bytes < 0 && errno == EINTR);
        int readErr = errno;
        close(failPipe[0]);
        while (waitpid(pid, nullptr, 0) < 0 && errno == EINTR) { }
        if (bytes == sizeof(err)) {
            jniThrowErrnoException(env, "launchProcess", err);
        } else if (bytes < 0) {
            jniThrowErrnoException(env, "read", readErr);
        } else if (bytes != 0) {
            THROW_EXCEPTION_WITH_MESSAGE(env, "java/io/IOException", "(Ljava/lang/String;)V",
                                         "Unexpected exec failure pipe result");
        }
    }();

    for (jsize i = 0; i < argc; ++i) {
        if (argv[i] != nullptr) env->ReleaseStringUTFChars(strings[i], argv[i]);
        if (strings[i] != nullptr) env->DeleteLocalRef(strings[i]);
    }
}

struct android_net_UidOwnerValue {
    [[maybe_unused]] int32_t iif;
    uint32_t rule;
};

inline long bpf(enum bpf_cmd cmd, const bpf_attr& attr) {
    return syscall(__NR_bpf, cmd, &attr, sizeof(attr));
}

static long map_fd = -1L;

// based on: https://cs.android.com/android/platform/superproject/+/main:packages/modules/Connectivity/staticlibs/native/bpfmapjni/com_android_net_module_util_BpfMap.cpp;l=34;drc=9eca02a8fa20aa14920f0dd3bf88c06ce04a2575
// https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/service/native/TrafficController.cpp#190
extern "C" JNIEXPORT jboolean JNICALL
Java_be_mygod_vpnhotspot_root_Jni_removeUidInterfaceRules(JNIEnv *env, [[maybe_unused]] jobject obj,
                                                          jstring path, jint uid, jlong rules) {
    // mapRetrieveLocklessRW to bypass locking
    if (map_fd < 0) {
        const char *pathname = env->GetStringUTFChars(path, nullptr);
        // https://android.googlesource.com/platform/frameworks/libs/net/+/android-13.0.0_r1/common/native/bpf_syscall_wrappers/include/BpfSyscallWrappers.h#100
        map_fd = bpf(BPF_OBJ_GET, {
                .pathname = (uint64_t)pathname,
        });
        env->ReleaseStringUTFChars(path, pathname);
        if (map_fd < 0) {
            int err = errno;
            if (err != ENOSYS) jniThrowErrnoException(env, "BPF_OBJ_GET", err);
            return false;
        }
    }
    android_net_UidOwnerValue value;
    // findMapEntry
    if (bpf(BPF_MAP_LOOKUP_ELEM, {
            .map_fd = (uint32_t)map_fd,
            .key = (uint64_t)&uid,
            .value = (uint64_t)&value,
    })) {
        if (errno != ENOENT) jniThrowErrnoException(env, "BPF_MAP_LOOKUP_ELEM", errno);
        return false;
    }
    if (!(value.rule & rules)) return false;
    if (value.rule &= ~rules) {
        // writeToMapEntry
        if (bpf(BPF_MAP_UPDATE_ELEM, {
                .map_fd = (uint32_t)map_fd,
                .key = (uint64_t)&uid,
                .value = (uint64_t)&value,
                .flags = BPF_ANY,
        })) jniThrowErrnoException(env, "BPF_MAP_UPDATE_ELEM", errno);
        // deleteMapEntry
    } else if (bpf(BPF_MAP_DELETE_ELEM, {
            .map_fd = (uint32_t)map_fd,
            .key = (uint64_t)&uid,
    })) {
        int err = errno;
        if (err != ENOENT) jniThrowErrnoException(env, "BPF_MAP_DELETE_ELEM", err);
    }
    return true;
}
