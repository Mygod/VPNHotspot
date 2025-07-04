#!/bin/bash

# WebServer 功能测试脚本
# 使用方法：在手机上安装应用后，通过蓝牙或WiFi连接，然后运行此脚本

echo "=== VPNHotspot WebServer 功能测试 ==="
echo ""

# 获取手机IP地址（假设通过USB连接）
PHONE_IP="192.168.42.129"  # 默认USB网络共享IP，可能需要调整
PORT="9999"

echo "测试目标: http://$PHONE_IP:$PORT"
echo ""

# 测试1: 检查WebServer是否响应
echo "1. 测试WebServer连接..."
if curl -s --connect-timeout 5 "http://$PHONE_IP:$PORT" > /dev/null; then
    echo "   ✓ WebServer 连接成功"
else
    echo "   ✗ WebServer 连接失败"
    echo "   请检查："
    echo "   - 手机是否已安装并启动VPNHotspot应用"
    echo "   - 是否通过USB/蓝牙/WiFi连接到手机"
    echo "   - 手机IP地址是否正确（当前使用: $PHONE_IP）"
    exit 1
fi

# 测试2: 获取状态信息
echo ""
echo "2. 测试状态API..."
STATUS_RESPONSE=$(curl -s "http://$PHONE_IP:$PORT/status")
if [ $? -eq 0 ]; then
    echo "   ✓ 状态API响应成功"
    echo "   响应内容: $STATUS_RESPONSE"
else
    echo "   ✗ 状态API响应失败"
fi

# 测试3: 测试WiFi热点控制（仅测试API响应，不实际启动）
echo ""
echo "3. 测试WiFi热点控制API..."
START_RESPONSE=$(curl -s -X POST "http://$PHONE_IP:$PORT/wifi/start")
if [ $? -eq 0 ]; then
    echo "   ✓ WiFi启动API响应成功"
    echo "   响应内容: $START_RESPONSE"
else
    echo "   ✗ WiFi启动API响应失败"
fi

echo ""
echo "=== 测试完成 ==="
echo ""
echo "如果所有测试都通过，你可以在浏览器中访问："
echo "http://$PHONE_IP:$PORT"
echo ""
echo "来使用完整的网页控制界面。" 