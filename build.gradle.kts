plugins {
    id("com.android.application") version "8.5.1" apply false
    id("com.github.ben-manes.versions") version "0.51.0"
    id("com.google.devtools.ksp") version "2.0.0-1.0.23" apply false
    id("org.jetbrains.kotlin.android") version "2.0.0" apply false
}

buildscript {
    dependencies {
        classpath("com.google.firebase:firebase-crashlytics-gradle:3.0.2")
        classpath("com.google.android.gms:oss-licenses-plugin:0.10.6")
        classpath("com.google.gms:google-services:4.4.2")
    }
}
