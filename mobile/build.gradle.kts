import groovy.json.JsonOutput
import org.gradle.api.DefaultTask
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputDirectory
import org.gradle.api.tasks.Internal
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import org.gradle.api.tasks.compile.JavaCompile
import org.gradle.jvm.tasks.Jar

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.compose.compiler)
    alias(libs.plugins.crashlytics)
    alias(libs.plugins.ksp)
    alias(libs.plugins.google.services)
    alias(libs.plugins.kotlin.parcelize)
    alias(libs.plugins.wire)
    id("com.google.android.gms.oss-licenses-plugin")
}

abstract class GenerateGitJavaTask : DefaultTask() {
    @get:Input
    abstract val includeStatus: Property<Boolean>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Internal
    abstract val repositoryDir: DirectoryProperty

    @TaskAction
    fun generate() {
        val repositoryDir = repositoryDir.get().asFile
        val gitSha = try {
            ProcessBuilder("git", "rev-parse", "HEAD").directory(repositoryDir).redirectErrorStream(true).start().run {
                inputStream.bufferedReader().readText().trimEnd().takeIf { waitFor() == 0 }
            }
        } catch (_: Exception) {
            null
        }
        val gitStatus = if (includeStatus.get()) try {
            ProcessBuilder("git", "status", "--porcelain=v1").directory(repositoryDir).redirectErrorStream(true)
                .start().run { inputStream.bufferedReader().readText().trimEnd().takeIf { waitFor() == 0 } }
        } catch (_: Exception) {
            null
        } else null
        outputDir.file("be/mygod/vpnhotspot/BuildGit.java").get().asFile.apply {
            parentFile.mkdirs()
            writeText("""
                package be.mygod.vpnhotspot;
                public final class BuildGit {
                    public static final String VALUE = ${JsonOutput.toJson(
                if (gitSha.isNullOrEmpty()) "" else if (gitStatus.isNullOrEmpty()) gitSha else "$gitSha\n$gitStatus")};
                    private BuildGit() {}
                }
            """.trimIndent() + "\n")
        }
    }
}

abstract class BuildDaemonNativeLibsTask : DefaultTask() {
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val sourceDir: DirectoryProperty

    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val protoDir: DirectoryProperty

    @get:Input
    abstract val cargoProfile: Property<String>

    @get:Input
    abstract val androidPlatform: Property<Int>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Internal
    abstract val targetDir: DirectoryProperty

    @TaskAction
    fun build() {
        val cargoDir = sourceDir.get().asFile
        val targetDir = targetDir.get().asFile
        outputDir.get().asFile.apply {
            deleteRecursively()
            mkdirs()
        }
        val profile = cargoProfile.get()
        val rustFlags = listOf(
            "--remap-path-prefix=${System.getProperty("user.home").trimEnd('/')}/=",
            "--remap-path-prefix=${cargoDir.absolutePath}=.",
            "--remap-path-prefix=${cargoDir.absolutePath}/=",
        ).joinToString("\u001F")
        val targets = listOf(
            "arm64-v8a" to "aarch64-linux-android",
            "armeabi-v7a" to "armv7-linux-androideabi",
            "x86" to "i686-linux-android",
            "x86_64" to "x86_64-linux-android",
        )
        for ((abi, target) in targets) {
            val command = mutableListOf("cargo", "ndk", "--target", abi, "--platform", androidPlatform.get().toString(),
                "build", "--locked", "--bin", "vpnhotspotd").apply {
                if (profile == "release") add("--release")
            }
            val process = ProcessBuilder(command).directory(cargoDir).redirectErrorStream(true).apply {
                environment().run {
                    this["CARGO_BUILD_TARGET_DIR"] = targetDir.absolutePath
                    this["CARGO_ENCODED_RUSTFLAGS"] = rustFlags
                }
            }.start()
            val output = process.inputStream.bufferedReader().readText()
            check(process.waitFor() == 0) {
                "cargo build failed for $target\n$output"
            }
            val binary = targetDir.resolve("$target/$profile/vpnhotspotd")
            check(binary.isFile) { "Missing daemon binary: ${binary.absolutePath}" }
            outputDir.file("$abi/libvpnhotspotd.so").get().asFile.apply {
                parentFile.mkdirs()
                binary.copyTo(this, overwrite = true)
            }
        }
    }
}

val javaVersion = 11
android {
    namespace = "be.mygod.vpnhotspot"

    compileOptions {
        sourceCompatibility(javaVersion)
        targetCompatibility(javaVersion)
    }
    compileSdk = 37
    defaultConfig {
        applicationId = "be.mygod.vpnhotspot"
        minSdk = 29
        targetSdk = 36
        versionCode = 2002
        versionName = "3.0.1"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }
    splits {
        abi {
            isEnable = true
            reset()
            include("arm64-v8a", "armeabi-v7a", "x86", "x86_64")
            isUniversalApk = false
        }
    }
    buildFeatures {
        buildConfig = true
        compose = true
    }
    buildTypes {
        debug {
            isPseudoLocalesEnabled = true
        }
        release {
            isShrinkResources = true
            isMinifyEnabled = true
            // BuildGit already records the commit; AGP's VCS tag task fails when Git packs branch refs.
            vcsInfo.include = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    packaging.resources.excludes += listOf(
        "**/*.kotlin_*",
        "META-INF/versions/**",
    )
    lint.warning += "FullBackupContent"
    lint.warning += "UnsafeOptInUsageError"
    sourceSets.getByName("androidTest").assets.directories.add("$projectDir/schemas")
}
val compileHiddenApiStubs by tasks.registering(JavaCompile::class) {
    source("src/hiddenApiStubs/java")
    classpath = files(androidComponents.sdkComponents.bootClasspath)
    destinationDirectory.set(layout.buildDirectory.dir("intermediates/hiddenApiStubs/classes"))
    sourceCompatibility = javaVersion.toString()
    targetCompatibility = javaVersion.toString()
    options.release.set(javaVersion)
}
val hiddenApiStubsClasses = files(compileHiddenApiStubs.flatMap { it.destinationDirectory })
    .builtBy(compileHiddenApiStubs)
val hiddenApiStubsJar by tasks.registering(Jar::class) {
    from(hiddenApiStubsClasses)
    archiveFileName.set("hidden-api-stubs.jar")
}
wire {
    kotlin {
        enumMode = "sealed_class"
        rpcRole = "none"
    }
}
androidComponents.onVariants { variant ->
    val variantTitle = variant.name.replaceFirstChar(Char::titlecase)
    val task = tasks.register<GenerateGitJavaTask>("generate${variantTitle}GitJava") {
        includeStatus.set(variant.buildType == "debug")
        outputDir.set(layout.buildDirectory.dir("generated/source/git/${variant.name}"))
        repositoryDir.set(rootProject.layout.projectDirectory)
        outputs.upToDateWhen { false }
    }
    variant.sources.java?.addGeneratedSourceDirectory(task, GenerateGitJavaTask::outputDir)
    val daemonTask = tasks.register<BuildDaemonNativeLibsTask>(
        "build${variantTitle}DaemonNativeLibs") {
        sourceDir.set(layout.projectDirectory.dir("src/main/rust/vpnhotspotd"))
        protoDir.set(layout.projectDirectory.dir("src/main/proto"))
        cargoProfile.set(if (variant.buildType == "release") "release" else "debug")
        androidPlatform.set(android.defaultConfig.minSdk!!)
        outputDir.set(layout.buildDirectory.dir("generated/nativeLibs/daemon/${variant.name}"))
        targetDir.set(layout.buildDirectory.dir("rust/vpnhotspotd"))
    }
    variant.sources.jniLibs?.addGeneratedSourceDirectory(daemonTask, BuildDaemonNativeLibsTask::outputDir)
}
ksp {
    arg("room.expandProjection", "true")
    arg("room.incremental", "true")
    arg("room.schemaLocation", "$projectDir/schemas")
}

dependencies {
    compileOnly(files(hiddenApiStubsJar))
    ksp(libs.room.compiler)
    implementation(platform(libs.compose.bom))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material3.adaptive:adaptive")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation(libs.activity.compose)
    implementation(libs.browser)
    implementation(libs.core.ktx)
    implementation(libs.firebase.analytics)
    implementation(libs.firebase.crashlytics)
    implementation(libs.hiddenapibypass)
    implementation(libs.ktor.io.jvm)
    implementation(libs.kotlinx.collections.immutable)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.librootkotlinx)
    implementation(libs.lifecycle.runtime.compose)
    implementation(libs.lifecycle.viewmodel.compose)
    implementation(libs.navigation.compose)
    implementation(libs.play.services.oss.licenses)
    implementation(libs.room.ktx)
    implementation(libs.timber)
    implementation(libs.wire.runtime)
    implementation(libs.zxing.core)
    debugImplementation(libs.leakcanary.android)
    debugImplementation("androidx.compose.ui:ui-tooling")
    testImplementation(libs.junit)
    androidTestImplementation(platform(libs.compose.bom))
    androidTestImplementation(libs.espresso.core)
    androidTestImplementation(libs.junit.ktx)
    androidTestImplementation(libs.room.testing)
    androidTestImplementation(libs.test.runner)
}
