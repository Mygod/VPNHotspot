plugins {
    id("com.github.ben-manes.versions") version "0.28.0"
}

buildscript {
    val kotlinVersion = "1.3.72"
    extra.set("kotlinVersion", kotlinVersion)

    repositories {
        google()
        jcenter()
    }

    dependencies {
        classpath("com.android.tools.build:gradle:4.0.0")
        classpath("com.github.ben-manes:gradle-versions-plugin:0.28.0")
        classpath("com.google.firebase:firebase-crashlytics-gradle:2.1.1")
        classpath("com.google.android.gms:oss-licenses-plugin:0.10.2")
        classpath("com.google.gms:google-services:4.3.3")
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:$kotlinVersion")
    }
}

allprojects {
    repositories {
        google()
        jcenter()
        maven("https://jitpack.io")
    }
}

tasks.register<Delete>("clean") {
    delete(rootProject.buildDir)
}
