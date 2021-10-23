plugins {
    id("com.github.ben-manes.versions") version "0.39.0"
}

buildscript {
    repositories {
        google()
        mavenCentral()
    }

    dependencies {
        classpath(kotlin("gradle-plugin", "1.5.31"))
        classpath("com.android.tools.build:gradle:7.0.3")
        classpath("com.google.firebase:firebase-crashlytics-gradle:2.7.1")
        classpath("com.google.android.gms:oss-licenses-plugin:0.10.4")
        classpath("com.google.gms:google-services:4.3.10")
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
