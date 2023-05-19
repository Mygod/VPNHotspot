plugins {
    id("com.android.application") version "8.2.0-alpha04" apply false
    id("com.github.ben-manes.versions") version "0.46.0"
    id("org.jetbrains.kotlin.android") version "1.8.21" apply false
}

buildscript {
    dependencies {
        classpath("com.google.firebase:firebase-crashlytics-gradle:2.9.5")
        classpath("com.google.android.gms:oss-licenses-plugin:0.10.6")
        classpath("com.google.gms:google-services:4.3.15")
    }
}
