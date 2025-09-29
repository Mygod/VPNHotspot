# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

VPNHotspot is an Android application that enables VPN sharing over WiFi hotspot/repeater with root access. It provides system-level networking controls and includes a built-in web server for remote management.

## Architecture

### Core Components

- **App.kt**: Main Application class with Firebase initialization and device storage management
- **MainActivity.kt**: Entry point with navigation and fragment management
- **OkHttpWebServer.kt**: Custom HTTP server built on OkHttp for remote control (port 9999)
- **ApiKeyManager.kt**: Authentication system for web/API access

### Domain Structure

- **net/**: Network management (tethering, routing, DNS, WiFi)
  - `TetheringManagerCompat.kt`: Android version compatibility layer
  - `WifiApManager.kt`: WiFi access point management
  - `dns/DnsForwarder.kt`: DNS forwarding functionality
- **client/**: Connected device monitoring and management
- **manage/**: Tethering control interfaces (WiFi, Bluetooth, USB, Ethernet)
- **room/**: SQLite database for client records and traffic monitoring
- **root/**: Root commands and JNI interface
- **tasker/**: Tasker plugin integration

### Web Server Features

- **Port**: 9999 (configurable)
- **Endpoints**:
  - `/api/status` - System status (battery, CPU, WiFi)
  - `/api/wifi/start` - Start WiFi tethering
  - `/api/wifi/stop` - Stop WiFi tethering
  - `/api/generate-key` - Generate API key (developer mode)
- **Authentication**: Optional API key via URL path, headers, or query params
- **Access**: LAN only via Bluetooth PAN, WiFi hotspot, or USB tethering

## Development Setup

### Build Commands

```bash
# Build debug APK
./gradlew assembleDebug

# Build release APK
./gradlew assembleRelease

# Run lint
./gradlew lint

# Run unit tests
./gradlew test

# Run Android tests
./gradlew connectedAndroidTest

# Run detekt
./gradlew detekt

# Check for dependency updates
./gradlew dependencyUpdates
```

### Testing

- **Unit tests**: Located in `mobile/src/test/`
- **Android tests**: Located in `mobile/src/androidTest/`
- **Web server test**: Use `test_webserver.sh` script for manual testing

### Key Dependencies

- **Kotlin**: 2.1.10
- **Android SDK**: minSdk 28, targetSdk 35, compileSdk 36
- **Architecture**: ViewModel, LiveData, Room, Navigation
- **Networking**: OkHttp, Ktor
- **Root access**: librootkotlinx
- **Firebase**: Analytics, Crashlytics

### Development Notes

- **Root required**: Most features require root access via su
- **System app**: Some features need `/system/priv-app` installation
- **Hidden APIs**: Uses restricted Android APIs via reflection
- **Multi-flavor**: `freedom` (F-Droid) and `google` (Play Store) variants

### Common Development Tasks

1. **Add new API endpoint**: Extend `OkHttpWebServer.kt` processRequest method
2. **Modify tethering behavior**: Update classes in `net/` and `manage/` packages
3. **Database changes**: Update Room entities in `room/` and run migrations
4. **UI changes**: Modify fragments in respective packages, update layouts in `res/layout/`

### Debugging

- **Web server logs**: Check logcat for "OkHttpWebServer" tag
- **Root commands**: Enable verbose logging in `RootManager.kt`
- **Network issues**: Use `adb shell dumpsys connectivity` for tethering state
- **API testing**: Use curl or browser with phone's tethering IP:9999

### 请每次回我中文！！！
- 当遇到系统没有的包时，先尝试判断系统并安装对应的包，比如python的pip安装requests 比如brew 安装指定的java版本

