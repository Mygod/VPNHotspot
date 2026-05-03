plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.ben.manes.versions)
    alias(libs.plugins.crashlytics) apply false
    alias(libs.plugins.google.services) apply false
    alias(libs.plugins.kotlin.android) apply false
    alias(libs.plugins.ksp) apply false
}

buildscript {
    dependencies {
        classpath("com.google.android.gms:oss-licenses-plugin:0.11.0")
    }
}
