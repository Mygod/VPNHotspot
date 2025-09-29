#!/bin/bash

# VPNHotspot 自动连接设置测试脚本
# 测试设置开关和远程控制功能

echo "=== VPNHotspot 自动连接设置测试 ==="
echo "测试时间: $(date)"
echo "ADB设备: 192.168.1.133:5555"
echo

# 检查ADB连接
echo "1. 检查ADB连接..."
adb connect 192.168.1.133:5555
if [ $? -ne 0 ]; then
    echo "❌ ADB连接失败，请检查设备"
    exit 1
fi
echo "✅ ADB连接成功"

# 安装应用（如果已安装则重新安装）
echo "2. 安装/更新应用..."
./gradlew installDebug
if [ $? -ne 0 ]; then
    echo "❌ 应用安装失败"
    exit 1
fi
echo "✅ 应用安装成功"

# 等待应用启动
echo "3. 等待应用启动..."
sleep 3

# 测试设置开关功能
echo "4. 测试设置开关功能..."

# 启用自动连接
echo "  4.1 启用自动连接开关..."
adb shell am start -n be.mygod.vpnhotspot/.SettingsActivity
sleep 2
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_ENTER

echo "  4.2 验证设置值..."
adb shell "am start -a android.intent.action.VIEW -d 'content://be.mygod.vpnhotspot.provider/shared_prefs/be.mygod.vpnhotspot_preferences.xml'"

# 等待设置生效
sleep 2

# 测试远程控制页面
echo "5. 测试远程控制页面..."
adb shell am start -n be.mygod.vpnhotspot/.MainActivity
sleep 2
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_ENTER

echo "6. 检查自动连接行为..."
adb shell "logcat -d | grep RemoteControl"

echo "7. 禁用自动连接开关..."
adb shell am start -n be.mygod.vpnhotspot/.SettingsActivity
sleep 2
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_DPAD_DOWN
adb shell input keyevent KEYCODE_ENTER

echo "8. 验证设置值..."
adb shell "am start -a android.intent.action.VIEW -d 'content://be.mygod.vpnhotspot.provider/shared_prefs/be.mygod.vpnhotspot_preferences.xml'"

echo
echo "=== 测试完成 ==="
echo "请手动验证："
echo "1. 打开设置页面，查看'远程控制自动连接'开关状态"
echo "2. 打开远程控制页面，检查是否根据开关状态自动连接"
echo "3. 切换开关状态，重新打开远程控制页面验证行为"