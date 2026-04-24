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
import java.util.Properties

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

abstract class BuildDaemonAssetsTask : DefaultTask() {
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val sourceDir: DirectoryProperty

    @get:Input
    abstract val cargoProfile: Property<String>

    @get:Input
    abstract val hostTag: Property<String>

    @get:Input
    abstract val ndkDir: Property<String>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @TaskAction
    fun build() {
        val cargoDir = sourceDir.get().asFile
        val targetDir = project.layout.buildDirectory.dir("rust/vpnhotspotd").get().asFile
        val toolchainDir = project.file("${ndkDir.get()}/toolchains/llvm/prebuilt/${hostTag.get()}/bin")
        check(toolchainDir.isDirectory) { "Missing NDK toolchain: ${toolchainDir.absolutePath}" }
        outputDir.get().asFile.apply {
            deleteRecursively()
            mkdirs()
        }
        val profile = cargoProfile.get()
        val targets = listOf(
            Triple("arm64-v8a", "aarch64-linux-android", "aarch64-linux-android29-clang"),
            Triple("armeabi-v7a", "armv7-linux-androideabi", "armv7a-linux-androideabi29-clang"),
            Triple("x86", "i686-linux-android", "i686-linux-android29-clang"),
            Triple("x86_64", "x86_64-linux-android", "x86_64-linux-android29-clang"),
        )
        for ((abi, target, linker) in targets) {
            val command = mutableListOf("cargo", "build", "--offline", "--target", target).apply {
                if (profile == "release") add("--release")
            }
            val process = ProcessBuilder(command).directory(cargoDir).redirectErrorStream(true).apply {
                environment()["CARGO_BUILD_TARGET_DIR"] = targetDir.absolutePath
                environment()["CARGO_TARGET_${target.uppercase().replace('-', '_')}_LINKER"] =
                    toolchainDir.resolve(linker).absolutePath
            }.start()
            val output = process.inputStream.bufferedReader().readText()
            check(process.waitFor() == 0) {
                "cargo build failed for $target\n$output"
            }
            val binary = targetDir.resolve("$target/$profile/vpnhotspotd")
            check(binary.isFile) { "Missing daemon binary: ${binary.absolutePath}" }
            outputDir.file("daemon/$abi/vpnhotspotd").get().asFile.apply {
                parentFile.mkdirs()
                binary.copyTo(this, overwrite = true)
            }
        }
    }
}

fun resolveAndroidNdkDir(project: org.gradle.api.Project): String {
    val properties = Properties()
    project.rootProject.file("local.properties").inputStream().use(properties::load)
    val sdkDir = properties.getProperty("sdk.dir")?.let(project::file)
        ?: error("sdk.dir is missing from local.properties")
    return sdkDir.resolve("ndk").listFiles()?.filter {
        it.isDirectory && it.name.firstOrNull()?.isDigit() == true
    }?.maxByOrNull { it.name }?.absolutePath
        ?: sdkDir.resolve("ndk-bundle").takeIf { it.isDirectory }?.absolutePath
        ?: error("Android NDK not found under ${sdkDir.absolutePath}")
}

fun resolveNdkHostTag() = when {
    System.getProperty("os.name").startsWith("Linux") -> "linux-x86_64"
    System.getProperty("os.name").startsWith("Mac") && System.getProperty("os.arch") == "aarch64" -> "darwin-arm64"
    System.getProperty("os.name").startsWith("Mac") -> "darwin-x86_64"
    else -> error("Unsupported host OS: ${System.getProperty("os.name")}")
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
        minSdk = 28
        targetSdk = 36
        versionCode = 1036
        versionName = "2.19.2"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        androidResources.localeFilters += listOf("es", "it", "ja", "pt-rBR", "ru", "zh-rCN", "zh-rTW")
        externalNativeBuild.cmake.arguments += listOf("-DANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES=ON")
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
            vcsInfo.include = true
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
    externalNativeBuild.cmake.path = file("src/main/cpp/CMakeLists.txt")
}
androidComponents.onVariants { variant ->
    val task = tasks.register<GenerateGitJavaTask>("generate${variant.name.replaceFirstChar(Char::titlecase)}GitJava") {
        includeStatus.set(variant.buildType == "debug")
        outputDir.set(layout.buildDirectory.dir("generated/source/git/${variant.name}"))
        outputs.upToDateWhen { false }
    }
    variant.sources.java?.addGeneratedSourceDirectory(task, GenerateGitJavaTask::outputDir)
    val daemonTask = tasks.register<BuildDaemonAssetsTask>(
        "build${variant.name.replaceFirstChar(Char::titlecase)}DaemonAssets") {
        sourceDir.set(layout.projectDirectory.dir("src/main/rust/vpnhotspotd"))
        cargoProfile.set(if (variant.buildType == "release") "release" else "debug")
        hostTag.set(resolveNdkHostTag())
        ndkDir.set(resolveAndroidNdkDir(project))
        outputDir.set(layout.buildDirectory.dir("generated/assets/daemon/${variant.name}"))
    }
    variant.sources.assets?.addGeneratedSourceDirectory(daemonTask, BuildDaemonAssetsTask::outputDir)
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
    implementation(libs.dnsjava)
    implementation(libs.firebase.analytics)
    implementation(libs.firebase.crashlytics)
    implementation(libs.fragment.ktx)
    implementation(libs.hiddenapibypass)
    implementation(libs.ktor.network.jvm)
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
