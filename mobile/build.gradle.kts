plugins {
    id("com.android.application")
    id("com.google.android.gms.oss-licenses-plugin")
    id("com.google.gms.google-services")
    id("com.google.firebase.crashlytics")
    kotlin("android")
    kotlin("kapt")
    id("kotlin-parcelize")
}

android {
    val javaVersion = JavaVersion.VERSION_1_8
    val targetSdk = 29
    buildToolsVersion = "30.0.3"
    compileOptions {
        isCoreLibraryDesugaringEnabled = true
        sourceCompatibility = javaVersion
        targetCompatibility = javaVersion
    }
    compileSdk = 30
    kotlinOptions.jvmTarget = javaVersion.toString()
    defaultConfig {
        applicationId = "be.mygod.vpnhotspot"
        minSdk = 21
        this.targetSdk = targetSdk
        resourceConfigurations.addAll(arrayOf("it", "ru", "zh-rCN", "zh-rTW"))
        versionCode = 262
        versionName = "2.11.9"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        javaCompileOptions.annotationProcessorOptions.arguments.apply {
            put("room.expandProjection", "true")
            put("room.incremental", "true")
            put("room.schemaLocation", "$projectDir/schemas")
        }
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
    packagingOptions.resources.excludes.add("**/*.kotlin_*")
    flavorDimensions.add("freedom")
    productFlavors {
        create("freedom") {
            dimension = "freedom"
        }
        create("google") {
            dimension = "freedom"
            buildConfigField("boolean", "DONATIONS", "false")
        }
    }
    sourceSets.getByName("androidTest").assets.srcDir("$projectDir/schemas")
}

dependencies {
    val lifecycleVersion = "2.3.1"
    val roomVersion = "2.3.0"

    coreLibraryDesugaring("com.android.tools:desugar_jdk_libs:1.1.5")
    kapt("androidx.room:room-compiler:$roomVersion")
    implementation(kotlin("stdlib-jdk8"))
    implementation("androidx.appcompat:appcompat:1.3.0")    // https://issuetracker.google.com/issues/151603528
    implementation("androidx.browser:browser:1.3.0")
    implementation("androidx.core:core-ktx:1.6.0")
    implementation("androidx.emoji:emoji:1.1.0")
    implementation("androidx.fragment:fragment-ktx:1.3.5")
    implementation("androidx.lifecycle:lifecycle-common-java8:$lifecycleVersion")
    implementation("androidx.lifecycle:lifecycle-livedata-ktx:$lifecycleVersion")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:$lifecycleVersion")
    implementation("androidx.preference:preference:1.1.1")
    implementation("androidx.room:room-ktx:$roomVersion")
    implementation("androidx.swiperefreshlayout:swiperefreshlayout:1.1.0")
    implementation("com.android.billingclient:billing-ktx:4.0.0")
    implementation("com.google.android.gms:play-services-oss-licenses:17.0.0")
    implementation("com.google.android.material:material:1.4.0")
    implementation("com.google.firebase:firebase-analytics-ktx:19.0.0")
    implementation("com.google.firebase:firebase-crashlytics:18.1.0")
    implementation("com.google.zxing:core:3.4.1")
    implementation("com.jakewharton.timber:timber:4.7.1")
    implementation("com.linkedin.dexmaker:dexmaker:2.28.1")
    implementation("com.takisoft.preferencex:preferencex-simplemenu:1.1.0")
    implementation("org.jetbrains.kotlinx:kotlinx-collections-immutable:0.3.4")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.5.1")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.room:room-testing:$roomVersion")
    androidTestImplementation("androidx.test:runner:1.4.0")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.4.0")
    androidTestImplementation("androidx.test.ext:junit-ktx:1.1.3")
}
