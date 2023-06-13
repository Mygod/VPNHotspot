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

    val javaVersion = 11
    buildToolsVersion = "34.0.0"
    compileOptions {
        isCoreLibraryDesugaringEnabled = true
        sourceCompatibility(javaVersion)
        targetCompatibility(javaVersion)
    }
    kotlinOptions.jvmTarget = javaVersion.toString()
    tasks.withType<org.jetbrains.kotlin.gradle.tasks.KotlinCompile> {
        kotlinOptions.jvmTarget = javaVersion.toString()
    }
    compileSdk = 34
    defaultConfig {
        applicationId = "be.mygod.vpnhotspot"
        minSdk = 28
        targetSdk = 34
        resourceConfigurations.addAll(arrayOf("it", "pt-rBR", "ru", "zh-rCN", "zh-rTW"))
        versionCode = 1005
        versionName = "2.16.5"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        javaCompileOptions.annotationProcessorOptions.arguments.apply {
            put("room.expandProjection", "true")
            put("room.incremental", "true")
            put("room.schemaLocation", "$projectDir/schemas")
        }
    }
    buildFeatures {
        buildConfig = true
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
    packagingOptions.resources.excludes.addAll(listOf(
        "**/*.kotlin_*",
        "META-INF/versions/**",
    ))
    lint.warning += "UnsafeOptInUsageError"
    flavorDimensions.add("freedom")
    productFlavors {
        create("freedom") {
            dimension = "freedom"
        }
        create("google") {
            dimension = "freedom"
            versionNameSuffix = "-g"
        }
    }
    sourceSets.getByName("androidTest").assets.srcDir("$projectDir/schemas")
}

dependencies {
    val lifecycleVersion = "2.6.1"
    val roomVersion = "2.5.1"

    coreLibraryDesugaring("com.android.tools:desugar_jdk_libs:2.0.3")
    kapt("androidx.room:room-compiler:$roomVersion")
    implementation(kotlin("stdlib-jdk8"))
    implementation("androidx.browser:browser:1.5.0")
    implementation("androidx.core:core-ktx:1.11.0-beta02")
    implementation("androidx.fragment:fragment-ktx:1.6.0")
    implementation("androidx.lifecycle:lifecycle-livedata-ktx:$lifecycleVersion")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:$lifecycleVersion")
    implementation("androidx.preference:preference:1.2.0")
    implementation("androidx.room:room-ktx:$roomVersion")
    implementation("androidx.swiperefreshlayout:swiperefreshlayout:1.1.0")
    implementation("be.mygod.librootkotlinx:librootkotlinx:1.0.5")
    implementation("com.android.billingclient:billing-ktx:6.0.0")
    implementation("com.google.android.gms:play-services-base:18.2.0")  // fix for GoogleApiActivity crash @ 18.1.0+
    implementation("com.google.android.gms:play-services-oss-licenses:17.0.1")
    implementation("com.google.android.material:material:1.9.0")
    implementation("com.google.firebase:firebase-analytics-ktx:21.3.0")
    implementation("com.google.firebase:firebase-crashlytics:18.3.7")
    implementation("com.google.zxing:core:3.5.1")
    implementation("com.jakewharton.timber:timber:5.0.1")
    implementation("com.linkedin.dexmaker:dexmaker:2.28.3")
    implementation("com.takisoft.preferencex:preferencex-simplemenu:1.1.0")
    implementation("org.lsposed.hiddenapibypass:hiddenapibypass:4.3")
    implementation("org.jetbrains.kotlinx:kotlinx-collections-immutable:0.3.5")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.7.1")
    add("googleImplementation", "com.google.android.play:app-update-ktx:2.1.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.room:room-testing:$roomVersion")
    androidTestImplementation("androidx.test:runner:1.5.2")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.1")
    androidTestImplementation("androidx.test.ext:junit-ktx:1.1.5")
}
