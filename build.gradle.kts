plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.ben.manes.versions)
    alias(libs.plugins.compose.compiler) apply false
    alias(libs.plugins.crashlytics) apply false
    alias(libs.plugins.google.services) apply false
    alias(libs.plugins.kotlin.parcelize) apply false
    alias(libs.plugins.ksp) apply false
    alias(libs.plugins.wire) apply false
}

buildscript {
    dependencies {
        classpath(libs.oss.licenses.plugin)
    }
}
