plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

android {
    namespace = "build.agora.dialf_phone"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "build.agora.dialf_phone"
        // minSdk 29: RoleManager.ROLE_DIALER (default-dialer flow) is API 29+.
        minSdk = 29
        targetSdk = flutter.targetSdkVersion
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    signingConfigs {
        // Stable *sideload* key (committed, not a Play Store key). Gives every release APK
        // the same signature so sideloaded updates install over each other instead of
        // failing with INSTALL_FAILED_UPDATE_INCOMPATIBLE (the auto-generated debug key
        // differs per CI runner).
        create("sideload") {
            storeFile = file("dialf-sideload.keystore")
            storePassword = "dialfsideload"
            keyAlias = "dialf"
            keyPassword = "dialfsideload"
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.getByName("sideload")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}

dependencies {
    // Native WebSocket client for the headless control-plane service.
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
}
