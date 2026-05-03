import groovy.json.JsonOutput
import org.gradle.api.DefaultTask
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.InputDirectory
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.crashlytics)
    alias(libs.plugins.ksp)
    alias(libs.plugins.google.services)
    alias(libs.plugins.kotlin.android)
    id("com.google.android.gms.oss-licenses-plugin")
    kotlin("kapt")
    id("kotlin-parcelize")
}

abstract class GenerateGitJavaTask : DefaultTask() {
    @get:Input
    abstract val includeStatus: Property<Boolean>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @TaskAction
    fun generate() {
        val gitSha = try {
            ProcessBuilder("git", "rev-parse", "HEAD").directory(project.rootDir).redirectErrorStream(true).start().run {
                inputStream.bufferedReader().readText().trimEnd().takeIf { waitFor() == 0 }
            }
        } catch (_: Exception) {
            null
        }
        val gitStatus = if (includeStatus.get()) try {
            ProcessBuilder("git", "status", "--porcelain=v1").directory(project.rootDir).redirectErrorStream(true)
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

    @get:Input
    abstract val cargoProfile: Property<String>

    @get:Input
    abstract val androidPlatform: Property<Int>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @TaskAction
    fun build() {
        val cargoDir = sourceDir.get().asFile
        val targetDir = project.layout.buildDirectory.dir("rust/vpnhotspotd").get().asFile
        outputDir.get().asFile.apply {
            deleteRecursively()
            mkdirs()
        }
        val profile = cargoProfile.get()
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
                environment()["CARGO_BUILD_TARGET_DIR"] = targetDir.absolutePath
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
        isCoreLibraryDesugaringEnabled = true
        sourceCompatibility(javaVersion)
        targetCompatibility(javaVersion)
    }
    tasks.withType<org.jetbrains.kotlin.gradle.tasks.KotlinCompile> {
        compilerOptions.jvmTarget.set(JvmTarget.fromTarget(javaVersion.toString()))
    }
    compileSdk = 36
    compileSdkMinor = 1
    defaultConfig {
        applicationId = "be.mygod.vpnhotspot"
        minSdk = 29
        targetSdk = 36
        versionCode = 2000
        versionName = "3.0.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        androidResources.localeFilters += listOf("es", "it", "ja", "pt-rBR", "ru", "zh-rCN", "zh-rTW")
    }
    buildFeatures {
        buildConfig = true
        dataBinding = true
        viewBinding = true
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
androidComponents.onVariants { variant ->
    val task = tasks.register<GenerateGitJavaTask>("generate${variant.name.replaceFirstChar(Char::titlecase)}GitJava") {
        includeStatus.set(variant.buildType == "debug")
        outputDir.set(layout.buildDirectory.dir("generated/source/git/${variant.name}"))
        outputs.upToDateWhen { false }
    }
    variant.sources.java?.addGeneratedSourceDirectory(task, GenerateGitJavaTask::outputDir)
    val daemonTask = tasks.register<BuildDaemonNativeLibsTask>(
        "build${variant.name.replaceFirstChar(Char::titlecase)}DaemonNativeLibs") {
        sourceDir.set(layout.projectDirectory.dir("src/main/rust/vpnhotspotd"))
        cargoProfile.set(if (variant.buildType == "release") "release" else "debug")
        androidPlatform.set(android.defaultConfig.minSdk!!)
        outputDir.set(layout.buildDirectory.dir("generated/nativeLibs/daemon/${variant.name}"))
    }
    variant.sources.jniLibs?.addGeneratedSourceDirectory(daemonTask, BuildDaemonNativeLibsTask::outputDir)
}
ksp {
    arg("room.expandProjection", "true")
    arg("room.incremental", "true")
    arg("room.schemaLocation", "$projectDir/schemas")
}
kotlin.compilerOptions.jvmTarget.set(JvmTarget.fromTarget(javaVersion.toString()))

dependencies {
    coreLibraryDesugaring(libs.desugar.jdk.libs)
    ksp(libs.room.compiler)
    implementation(libs.browser)
    implementation(libs.core.i18n)
    implementation(libs.core.ktx)
    implementation(libs.dexmaker)
    implementation(libs.firebase.analytics)
    implementation(libs.firebase.crashlytics)
    implementation(libs.fragment.ktx)
    implementation(libs.hiddenapibypass)
    implementation(libs.ktor.io.jvm)
    implementation(libs.kotlinx.collections.immutable)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.librootkotlinx)
    implementation(libs.lifecycle.livedata.ktx)
    implementation(libs.lifecycle.runtime.ktx)
    implementation(libs.material)
    implementation(libs.play.services.oss.licenses)
    implementation(libs.preference)
    implementation(libs.preferencex.simplemenu)
    implementation(libs.room.ktx)
    implementation(libs.swiperefreshlayout)
    implementation(libs.timber)
    implementation(libs.zxing.core)
    testImplementation(libs.junit)
    androidTestImplementation(libs.espresso.core)
    androidTestImplementation(libs.junit.ktx)
    androidTestImplementation(libs.room.testing)
    androidTestImplementation(libs.test.runner)
}
