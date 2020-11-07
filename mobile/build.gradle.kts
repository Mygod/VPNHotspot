plugins {
    id("com.android.application")
    id("com.google.android.gms.oss-licenses-plugin")
    id("com.google.gms.google-services")
    id("com.google.firebase.crashlytics")
    kotlin("android")
    kotlin("android.extensions")
    kotlin("kapt")
}

android {
    val javaVersion = JavaVersion.VERSION_1_8
    val targetSdk = 29
    buildToolsVersion("30.0.2")
    compileOptions {
        isCoreLibraryDesugaringEnabled = true
        sourceCompatibility = javaVersion
        targetCompatibility = javaVersion
    }
    compileSdkVersion(30)
    kotlinOptions {
        freeCompilerArgs = listOf("-XXLanguage:+InlineClasses")
        jvmTarget = javaVersion.toString()
    }
    defaultConfig {
        applicationId = "be.mygod.vpnhotspot"
        minSdkVersion(21)
        targetSdkVersion(targetSdk)
        resConfigs(listOf("it", "ru", "zh-rCN", "zh-rTW"))
        versionCode = 255
        versionName = "2.11.2"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        javaCompileOptions.annotationProcessorOptions.arguments(mapOf(
                "room.incremental" to "true",
                "room.schemaLocation" to "$projectDir/schemas"))
        buildConfigField("boolean", "DONATIONS", "true")
        buildConfigField("int", "TARGET_SDK", targetSdk.toString())
    }
    buildFeatures {
        dataBinding = true
        viewBinding = true
    }
    buildTypes {
        getByName("debug") {
            isPseudoLocalesEnabled = true
        }
        getByName("release") {
            isShrinkResources = true
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android.txt"), "proguard-rules.pro")
        }
    }
    packagingOptions.exclude("**/*.kotlin_*")
    flavorDimensions("freedom")
    productFlavors {
        create("freedom") {
            dimension("freedom")
        }
        create("google") {
            dimension("freedom")
            buildConfigField("boolean", "DONATIONS", "false")
        }
    }
    sourceSets.getByName("androidTest") {
        assets.setSrcDirs(assets.srcDirs + files("$projectDir/schemas"))
    }
}

dependencies {
    val lifecycleVersion = "2.3.0-beta01"
    val roomVersion = "2.2.5"

    coreLibraryDesugaring("com.android.tools:desugar_jdk_libs:1.1.0")
    kapt("androidx.room:room-compiler:$roomVersion")
    implementation("androidx.appcompat:appcompat:1.3.0-alpha02")    // https://issuetracker.google.com/issues/151603528
    implementation("androidx.browser:browser:1.2.0")
    implementation("androidx.core:core-ktx:1.3.2")
    implementation("androidx.emoji:emoji:1.1.0")
    implementation("androidx.fragment:fragment-ktx:1.3.0-beta01")
    implementation("androidx.lifecycle:lifecycle-common-java8:$lifecycleVersion")
    implementation("androidx.lifecycle:lifecycle-livedata-ktx:$lifecycleVersion")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:$lifecycleVersion")
    implementation("androidx.preference:preference:1.1.1")
    implementation("androidx.room:room-ktx:$roomVersion")
    implementation("androidx.swiperefreshlayout:swiperefreshlayout:1.1.0")
    implementation("com.android.billingclient:billing-ktx:3.0.1")
    implementation("com.google.android.gms:play-services-oss-licenses:17.0.0")
    implementation("com.google.android.material:material:1.2.1")
    implementation("com.google.firebase:firebase-analytics-ktx:18.0.0")
    implementation("com.google.firebase:firebase-crashlytics:17.2.2")
    implementation("com.google.zxing:core:3.4.1")
    implementation("com.jakewharton.timber:timber:4.7.1")
    implementation("com.linkedin.dexmaker:dexmaker:2.28.0")
    implementation("com.takisoft.preferencex:preferencex-simplemenu:1.1.0")
    implementation("org.jetbrains.kotlinx:kotlinx-collections-immutable:0.3.3")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.4.0")
    testImplementation("junit:junit:4.13.1")
    androidTestImplementation("androidx.room:room-testing:$roomVersion")
    androidTestImplementation("androidx.test:runner:1.3.0")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.3.0")
    androidTestImplementation("androidx.test.ext:junit-ktx:1.1.2")
}
