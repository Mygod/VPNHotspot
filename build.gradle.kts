plugins {
    id("com.android.application") version "8.2.0-alpha13" apply false
    id("com.github.ben-manes.versions") version "0.47.0"
    id("com.google.devtools.ksp") version "1.9.0-1.0.12" apply false
    id("org.jetbrains.kotlin.android") version "1.9.0" apply false
}

buildscript {
    dependencies {
        classpath("com.google.firebase:firebase-crashlytics-gradle:2.9.7")
        classpath("com.google.android.gms:oss-licenses-plugin:0.10.6")
        classpath("com.google.gms:google-services:4.3.15")
    }
}
