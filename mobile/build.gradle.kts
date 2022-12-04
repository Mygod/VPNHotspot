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
    namespace = "be.mygod.vpnhotspot"

    val javaVersion = JavaVersion.VERSION_11
    buildToolsVersion = "33.0.0"
    compileOptions {
        isCoreLibraryDesugaringEnabled = true
        sourceCompatibility = javaVersion
        targetCompatibility = javaVersion
    }
    compileSdk = 33
    kotlinOptions.jvmTarget = javaVersion.toString()
    defaultConfig {
        applicationId = "be.mygod.vpnhotspot"
        minSdk = 21
        @android.annotation.SuppressLint("ExpiredTargetSdkVersion")
        targetSdk = 29
        resourceConfigurations.addAll(arrayOf("it", "ru", "zh-rCN", "zh-rTW"))
        versionCode = 302
        versionName = "2.15.2"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        javaCompileOptions.annotationProcessorOptions.arguments.apply {
            put("room.expandProjection", "true")
            put("room.incremental", "true")
            put("room.schemaLocation", "$projectDir/schemas")
        }
        buildConfigField("boolean", "DONATIONS", "true")
        buildConfigField("int", "TARGET_SDK", "29")
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
            targetSdk = 33
            versionNameSuffix = "-g"
            buildConfigField("boolean", "DONATIONS", "false")
            buildConfigField("int", "TARGET_SDK", "33")
        }
    }
    sourceSets.getByName("androidTest").assets.srcDir("$projectDir/schemas")
}

dependencies {
    val lifecycleVersion = "2.5.1"
    val roomVersion = "2.5.0-beta02"

    coreLibraryDesugaring("com.android.tools:desugar_jdk_libs:2.0.0")
    kapt("androidx.room:room-compiler:$roomVersion")
    implementation(kotlin("stdlib-jdk8"))
    implementation("androidx.browser:browser:1.5.0-alpha01")
    implementation("androidx.core:core-ktx:1.9.0")
    implementation("androidx.fragment:fragment-ktx:1.5.4")
    implementation("androidx.lifecycle:lifecycle-livedata-ktx:$lifecycleVersion")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:$lifecycleVersion")
    implementation("androidx.preference:preference:1.2.0")
    implementation("androidx.room:room-ktx:$roomVersion")
    implementation("androidx.swiperefreshlayout:swiperefreshlayout:1.1.0")
    implementation("be.mygod.librootkotlinx:librootkotlinx:1.0.0")
    implementation("com.android.billingclient:billing-ktx:5.1.0")
    implementation("com.google.android.gms:play-services-oss-licenses:17.0.0")
    implementation("com.google.android.material:material:1.7.0")
    implementation("com.google.firebase:firebase-analytics-ktx:21.2.0")
    implementation("com.google.firebase:firebase-crashlytics:18.3.2")
    implementation("com.google.zxing:core:3.5.1")
    implementation("com.jakewharton.timber:timber:5.0.1")
    implementation("com.linkedin.dexmaker:dexmaker:2.28.3")
    implementation("com.takisoft.preferencex:preferencex-simplemenu:1.1.0")
    implementation("org.jetbrains.kotlinx:kotlinx-collections-immutable:0.3.5")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.6.4")
    add("googleImplementation", "com.github.tiann:FreeReflection:3.1.0")
    add("googleImplementation", "com.google.android.play:core:1.10.3")
    add("googleImplementation", "com.google.android.play:core-ktx:1.8.1")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.room:room-testing:$roomVersion")
    androidTestImplementation("androidx.test:runner:1.5.1")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
    androidTestImplementation("androidx.test.ext:junit-ktx:1.1.4")
}
