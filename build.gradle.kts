plugins {
    id("com.github.ben-manes.versions") version "0.38.0"
}

buildscript {
    repositories {
        google()
        mavenCentral()
    }

    dependencies {
        classpath(kotlin("gradle-plugin", "1.5.0"))
        classpath("com.android.tools.build:gradle:4.2.0")
        classpath("com.google.firebase:firebase-crashlytics-gradle:2.5.2")
        classpath("com.google.android.gms:oss-licenses-plugin:0.10.4")
        classpath("com.google.gms:google-services:4.3.5")
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
