plugins {
    id("com.android.application") version "8.0.0-beta03" apply false
    id("com.github.ben-manes.versions") version "0.45.0"
    id("org.jetbrains.kotlin.android") version "1.8.10" apply false
}

buildscript {
    dependencies {
        classpath("com.google.firebase:firebase-crashlytics-gradle:2.9.4")
        classpath("com.google.android.gms:oss-licenses-plugin:0.10.6")
        classpath("com.google.gms:google-services:4.3.15")
    }
}
