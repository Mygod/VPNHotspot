#!/bin/bash

# VPNHotspot macOS构建脚本
# 自动设置macOS环境的Java路径

set -e

echo "🍎 VPNHotspot macOS Build Script"
echo "================================="

# 检查是否为macOS
if [[ "$OSTYPE" != "darwin"* ]]; then
    echo "❌ 错误：此脚本仅适用于macOS系统"
    echo "当前系统: $OSTYPE"
    exit 1
fi

# 检测系统架构
ARCH=$(uname -m)
echo "📱 系统架构: $ARCH"

# 设置Java 17路径（根据架构选择）
if [[ "$ARCH" == "arm64" ]]; then
    # Apple Silicon Macs
    JAVA_HOME_PATH="/opt/homebrew/Cellar/openjdk@17/17.0.9"
    echo "🔧 配置Apple Silicon Mac的Java路径"
elif [[ "$ARCH" == "x86_64" ]]; then
    # Intel Macs
    JAVA_HOME_PATH="/usr/local/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home"
    echo "🔧 配置Intel Mac的Java路径"
else
    echo "❌ 不支持的架构: $ARCH"
    exit 1
fi

# 检查Java路径是否存在
if [[ ! -d "$JAVA_HOME_PATH" ]]; then
    echo "❌ Java 17未找到，请通过Homebrew安装："
    echo "   brew install openjdk@17"
    exit 1
fi

# 设置环境变量
export JAVA_HOME="$JAVA_HOME_PATH"
export PATH="$JAVA_HOME/bin:$PATH"

echo "✅ Java 17路径: $JAVA_HOME"
echo "✅ Java版本: $(java -version 2>&1 | head -n 1)"

# 执行构建
echo ""
echo "🚀 开始构建VPNHotspot..."
./gradlew assembleFreedomDebug

echo ""
echo "✅ 构建完成！"
echo "📦 APK文件位置: mobile/build/outputs/apk/"