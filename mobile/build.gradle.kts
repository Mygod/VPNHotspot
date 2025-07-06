import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.aboutLibraries)
    alias(libs.plugins.android.application)
    alias(libs.plugins.crashlytics)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.ksp)
    alias(libs.plugins.google.services)
    kotlin("android")
    kotlin("kapt")
    id("kotlin-parcelize")
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
    defaultConfig {
        applicationId = "be.mygod.vpnhotspot"
        minSdk = 28
        targetSdk = 36
        versionCode = 1035
        versionName = "2.19.1"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        androidResources.localeFilters += listOf("es", "it", "ja", "pt-rBR", "ru", "zh-rCN", "zh-rTW")
        externalNativeBuild.cmake.arguments += listOf("-DANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES=ON")
    }
    buildFeatures {
        buildConfig = true
        dataBinding = true
        viewBinding = true
        compose = true
    }
    buildTypes {
        debug {
            isPseudoLocalesEnabled = true
        }
        release {
            isShrinkResources = true
            isMinifyEnabled = true
            vcsInfo.include = true
            proguardFiles(getDefaultProguardFile("proguard-android.txt"), "proguard-rules.pro")
        }
    }
    packagingOptions.resources.excludes.addAll(listOf(
        "**/*.kotlin_*",
        "META-INF/versions/**",
    ))
    lint.warning += "FullBackupContent"
    lint.warning += "UnsafeOptInUsageError"
    sourceSets.getByName("androidTest").assets.srcDir("$projectDir/schemas")
    externalNativeBuild.cmake.path = file("src/main/cpp/CMakeLists.txt")
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
    implementation(libs.aboutlibraries.compose.m3)
    implementation(libs.activity.compose)
    implementation(libs.browser)
    implementation(libs.core.i18n)
    implementation(libs.core.ktx)
    implementation(libs.dexmaker)
    implementation(libs.dnsjava)
    implementation(libs.firebase.analytics)
    implementation(libs.firebase.crashlytics)
    implementation(libs.foundation.layout)
    implementation(libs.fragment.ktx)
    implementation(libs.hiddenapibypass)
    implementation(libs.ktor.network.jvm)
    implementation(libs.kotlinx.collections.immutable)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.librootkotlinx)
    implementation(libs.lifecycle.livedata.ktx)
    implementation(libs.lifecycle.runtime.ktx)
    implementation(libs.material)
    implementation(libs.material3.android)
    implementation(libs.play.services.oss.licenses)
    implementation(libs.preference)
    implementation(libs.preferencex.simplemenu)
    implementation(libs.room.ktx)
    implementation(libs.swiperefreshlayout)
    implementation(libs.taskerpluginlibrary)
    implementation(libs.timber)
    implementation(libs.zxing.core)
    testImplementation(libs.junit)
    androidTestImplementation(libs.espresso.core)
    androidTestImplementation(libs.junit.ktx)
    androidTestImplementation(libs.room.testing)
    androidTestImplementation(libs.test.runner)
}
