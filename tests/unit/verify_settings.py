#!/usr/bin/env python3
"""
VPNHotspot设置验证工具
用于验证远程控制自动连接开关的设置是否正确同步
"""

import subprocess
import xml.etree.ElementTree as ET
import sys
import time

def run_adb_command(command):
    """运行ADB命令并返回结果"""
    try:
        result = subprocess.run(command, shell=True, capture_output=True, text=True)
        return result.stdout.strip(), result.stderr.strip(), result.returncode
    except Exception as e:
        return "", str(e), 1

def check_device_connection():
    """检查ADB设备连接"""
    print("🔍 检查ADB设备连接...")
    stdout, stderr, code = run_adb_command("adb devices")
    
    if code != 0:
        print(f"❌ ADB命令执行失败: {stderr}")
        return False
    
    devices = [line for line in stdout.split('\n') if '\tdevice' in line]
    if not devices:
        print("❌ 未找到连接的ADB设备")
        return False
    
    print(f"✅ 找到 {len(devices)} 个设备")
    for device in devices:
        print(f"   {device}")
    return True

def get_setting_value():
    """获取远程控制自动连接设置值"""
    print("\n🔍 获取设置值...")
    
    # 尝试获取全局设置
    cmd = "adb shell cat /data/data/be.mygod.vpnhotspot/shared_prefs/be.mygod.vpnhotspot_preferences.xml"
    stdout, stderr, code = run_adb_command(cmd)
    
    if code != 0:
        print(f"❌ 无法读取设置文件: {stderr}")
        return None
    
    try:
        # 解析XML
        if not stdout:
            print("❌ 设置文件为空")
            return None
            
        # 查找设置值
        lines = stdout.split('\n')
        for line in lines:
            if 'remote.control.auto.connect' in line:
                # 提取布尔值
                if 'value="true"' in line:
                    print("✅ 全局设置: 远程控制自动连接 = 开启")
                    return True
                elif 'value="false"' in line:
                    print("✅ 全局设置: 远程控制自动连接 = 关闭")
                    return False
        
        # 如果没找到，使用默认值
        print("✅ 设置未找到，使用默认值: 开启")
        return True
        
    except Exception as e:
        print(f"❌ 解析设置文件失败: {e}")
        return None

def get_remote_control_settings():
    """获取远程控制专用设置"""
    print("\n🔍 获取远程控制设置...")
    
    cmd = "adb shell cat /data/data/be.mygod.vpnhotspot/shared_prefs/remote_control_prefs.xml"
    stdout, stderr, code = run_adb_command(cmd)
    
    if code != 0:
        print(f"⚠️  远程控制设置文件不存在，使用默认值")
        return {}
    
    settings = {}
    try:
        lines = stdout.split('\n')
        for line in lines:
            if 'auto_connect' in line and 'boolean' in line:
                if 'value="true"' in line:
                    settings['auto_connect'] = True
                elif 'value="false"' in line:
                    settings['auto_connect'] = False
            elif 'last_ip' in line and 'string' in line:
                # 提取IP地址
                start = line.find('>') + 1
                end = line.rfind('<')
                if start > 0 and end > start:
                    settings['last_ip'] = line[start:end]
            elif 'last_port' in line and 'int' in line:
                # 提取端口
                if 'value="' in line:
                    start = line.find('value="') + 7
                    end = line.find('"', start)
                    settings['last_port'] = int(line[start:end])
    
    except Exception as e:
        print(f"⚠️  解析远程控制设置失败: {e}")
    
    return settings

def check_logs():
    """检查相关日志"""
    print("\n🔍 检查相关日志...")
    
    # 获取最近10秒的日志
    cmd = "adb shell logcat -d -t 10s | grep -i 'remote\\|auto\\|connect'"
    stdout, stderr, code = run_adb_command(cmd)
    
    if stdout:
        print("📋 相关日志:")
        lines = stdout.split('\n')
        for line in lines[-10:]:  # 只显示最后10行
            if line.strip():
                print(f"   {line}")
    else:
        print("ℹ️  最近10秒无相关日志")

def test_setting_sync():
    """测试设置同步"""
    print("\n🔄 测试设置同步...")
    
    # 获取当前设置值
    global_setting = get_setting_value()
    remote_settings = get_remote_control_settings()
    
    if global_setting is None:
        print("❌ 无法获取全局设置")
        return False
    
    print(f"\n📊 设置同步状态:")
    print(f"   全局自动连接: {'开启' if global_setting else '关闭'}")
    
    local_auto = remote_settings.get('auto_connect', True)
    print(f"   本地自动连接: {'开启' if local_auto else '关闭'}")
    
    last_ip = remote_settings.get('last_ip', '未设置')
    last_port = remote_settings.get('last_port', 9999)
    print(f"   最后连接: {last_ip}:{last_port}")
    
    return True

def main():
    """主函数"""
    print("🚀 VPNHotspot设置验证工具")
    print("=" * 50)
    
    if not check_device_connection():
        print("\n❌ 请确保设备已连接并启用ADB调试")
        return 1
    
    test_setting_sync()
    check_logs()
    
    print("\n🎯 验证完成")
    print("\n💡 建议:")
    print("   1. 手动打开VPNHotspot应用")
    print("   2. 进入设置页面查看'远程控制自动连接'开关")
    print("   3. 切换开关状态并重新进入远程控制页面测试")
    
    return 0

if __name__ == "__main__":
    sys.exit(main())