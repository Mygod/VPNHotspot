plugins {
    id("com.android.application") version "8.10.1" apply false
    id("com.github.ben-manes.versions") version "0.52.0"
    id("com.google.devtools.ksp") version "2.1.21-2.0.2" apply false
    alias(libs.plugins.kotlin.android) apply false
}

buildscript {
    dependencies {
        classpath("com.google.firebase:firebase-crashlytics-gradle:3.0.4")
        classpath("com.google.gms:google-services:4.4.2")
    }
}
