#!/bin/bash

# 测试自动连接功能的脚本
echo "=== VPNHotspot 自动连接功能测试脚本 ==="
echo

# 检查ADB设备连接
echo "1. 检查设备连接..."
adb devices
echo

# 获取当前的自动连接设置
echo "2. 检查当前的自动连接设置..."
adb shell settings get secure remote.control.auto.connect
echo

# 使用dumpsys检查偏好设置
echo "3. 检查应用偏好设置..."
adb shell "dumpsys package be.mygod.vpnhotspot | grep -i preference"
echo

# 检查日志中的自动连接相关信息
echo "4. 检查日志中的自动连接信息..."
echo "查找最近的自动连接相关日志:"
adb logcat -d | grep -i "remote.control.auto.connect" | tail -5
echo
echo "查找RemoteControl相关日志:"
adb logcat -d | grep -i "RemoteControl" | tail -5
echo

# 提供测试建议
echo "=== 测试建议 ==="
echo "1. 在设置中开启/关闭"远程控制自动连接"开关"
echo "2. 进入远程控制页面观察是否自动连接"
echo "3. 检查日志中的'RemoteControl: autoConnectEnabled'值"
echo "4. 确保设置和读取使用同一个SharedPreferences文件"
echo

echo "=== 调试命令 ==="
echo "查看完整日志: adb logcat | grep RemoteControl"
echo "清除日志: adb logcat -c"
echo "强制停止应用: adb shell am force-stop be.mygod.vpnhotspot"
echo "启动应用: adb shell am start -n be.mygod.vpnhotspot/.MainActivity"
echo

echo "测试完成！"