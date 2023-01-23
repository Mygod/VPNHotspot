plugins {
    id("com.github.ben-manes.versions") version "0.44.0"
}

buildscript {
    repositories {
        google()
        mavenCentral()
    }

    dependencies {
        classpath(kotlin("gradle-plugin", "1.8.0"))
        classpath("com.android.tools.build:gradle:7.4.0-rc01")  // fixed to prevent crash on Android 8.1-
        classpath("com.google.firebase:firebase-crashlytics-gradle:2.9.2")
        classpath("com.google.android.gms:oss-licenses-plugin:0.10.6")
        classpath("com.google.gms:google-services:4.3.15")
    }
}

allprojects {
    repositories {
        google()
        jcenter()
        mavenCentral()
        maven("https://jitpack.io")
    }
}

tasks.register<Delete>("clean") {
    delete(rootProject.buildDir)
}
