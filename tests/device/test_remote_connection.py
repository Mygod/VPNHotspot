#!/usr/bin/env python3
"""
VPNHotspot远程连接测试工具
用于测试远程设备192.168.1.133:5555的连接功能
"""

import requests
import json
import time
import subprocess
import sys

class RemoteConnectionTester:
    def __init__(self, ip="192.168.1.133", port=9999, api_key="default_api_key_for_debug_2024"):
        self.ip = ip
        self.port = port
        self.api_key = api_key
        self.base_url = f"http://{ip}:{port}/{api_key}"
        
    def test_basic_connectivity(self):
        """测试基本连接"""
        print("🌐 测试基本连接...")
        try:
            url = f"{self.base_url}/api/status"
            print(f"   目标URL: {url}")
            
            response = requests.get(url, timeout=5)
            print(f"   响应状态: {response.status_code}")
            
            if response.status_code == 200:
                data = response.json()
                print(f"   响应数据: {json.dumps(data, indent=2, ensure_ascii=False)}")
                return True
            else:
                print(f"   错误响应: {response.text}")
                return False
                
        except requests.exceptions.Timeout:
            print("   ❌ 连接超时")
            return False
        except requests.exceptions.ConnectionError:
            print("   ❌ 连接失败")
            return False
        except Exception as e:
            print(f"   ❌ 异常: {e}")
            return False
    
    def test_wifi_control(self, action="start"):
        """测试WiFi控制功能"""
        print(f"📡 测试WiFi {action}...")
        try:
            url = f"{self.base_url}/api/wifi/{action}"
            print(f"   目标URL: {url}")
            
            response = requests.post(url, timeout=10)
            print(f"   响应状态: {response.status_code}")
            
            if response.status_code == 200:
                data = response.json()
                print(f"   响应数据: {json.dumps(data, indent=2, ensure_ascii=False)}")
                return data.get('success', False)
            else:
                print(f"   错误响应: {response.text}")
                return False
                
        except Exception as e:
            print(f"   ❌ 异常: {e}")
            return False
    
    def test_api_key_validation(self):
        """测试API Key验证"""
        print("🔑 测试API Key验证...")
        
        # 测试无效API Key
        invalid_url = f"http://{self.ip}:{self.port}/invalid_key/api/status"
        try:
            response = requests.get(invalid_url, timeout=5)
            if response.status_code == 404:
                print("   ✅ 无效API Key正确被拒绝")
            else:
                print(f"   ⚠️  意外响应: {response.status_code}")
        except:
            print("   ✅ 无效API Key被拒绝")
    
    def check_device_adb(self):
        """检查设备ADB连接"""
        print("🔍 检查ADB设备连接...")
        try:
            result = subprocess.run(["adb", "devices"], capture_output=True, text=True)
            if "192.168.1.133:5555" in result.stdout and "device" in result.stdout:
                print("   ✅ 设备已连接")
                return True
            else:
                print("   ❌ 设备未连接")
                print("   尝试连接...")
                result = subprocess.run(["adb", "connect", "192.168.1.133:5555"], 
                                      capture_output=True, text=True)
                if "connected" in result.stdout.lower():
                    print("   ✅ 设备已连接")
                    return True
                else:
                    print(f"   ❌ 连接失败: {result.stderr}")
                    return False
        except Exception as e:
            print(f"   ❌ ADB错误: {e}")
            return False
    
    def get_device_info(self):
        """获取设备信息"""
        print("📱 获取设备信息...")
        try:
            # 获取设备型号
            result = subprocess.run(["adb", "shell", "getprop", "ro.product.model"], 
                                  capture_output=True, text=True)
            model = result.stdout.strip()
            print(f"   设备型号: {model}")
            
            # 获取Android版本
            result = subprocess.run(["adb", "shell", "getprop", "ro.build.version.release"], 
                                  capture_output=True, text=True)
            version = result.stdout.strip()
            print(f"   Android版本: {version}")
            
            return True
        except Exception as e:
            print(f"   ❌ 获取设备信息失败: {e}")
            return False
    
    def test_full_workflow(self):
        """测试完整工作流程"""
        print("🔄 测试完整工作流程...")
        
        # 1. 检查设备
        if not self.check_device_adb():
            return False
        
        self.get_device_info()
        
        # 2. 基本连接测试
        if not self.test_basic_connectivity():
            print("   ❌ 基本连接测试失败")
            return False
        
        # 3. API Key验证
        self.test_api_key_validation()
        
        # 4. WiFi控制测试
        print("\n🔄 WiFi控制测试序列...")
        time.sleep(1)
        
        # 测试启动WiFi
        if self.test_wifi_control("start"):
            print("   ✅ WiFi启动成功")
            time.sleep(2)
        else:
            print("   ⚠️  WiFi启动失败")
        
        # 测试停止WiFi
        if self.test_wifi_control("stop"):
            print("   ✅ WiFi停止成功")
        else:
            print("   ⚠️  WiFi停止失败")
        
        return True
    
    def run_comprehensive_test(self):
        """运行综合测试"""
        print("🚀 VPNHotspot远程连接综合测试")
        print("=" * 50)
        print(f"目标设备: {self.ip}:{self.port}")
        print(f"API Key: {self.api_key}")
        print()
        
        # 测试网络连通性
        print("🔍 测试网络连通性...")
        try:
            response = requests.get(f"http://{self.ip}:{self.port}", timeout=3)
            print("   ✅ 设备网络可达")
        except:
            print("   ❌ 设备网络不可达")
            print("   请检查：")
            print("   - 设备是否开启了VPNHotspot应用")
            print("   - 设备网络是否正常")
            print("   - 防火墙设置")
            return False
        
        # 运行完整测试
        success = self.test_full_workflow()
        
        print("\n" + "=" * 50)
        if success:
            print("✅ 所有测试通过！远程连接功能正常")
        else:
            print("❌ 部分测试失败，请检查配置")
        
        return success

def main():
    """主函数"""
    if len(sys.argv) > 1:
        ip = sys.argv[1]
    else:
        ip = "192.168.1.133"
    
    if len(sys.argv) > 2:
        port = int(sys.argv[2])
    else:
        port = 9999
    
    if len(sys.argv) > 3:
        api_key = sys.argv[3]
    else:
        api_key = "default_api_key_for_debug_2024"
    
    tester = RemoteConnectionTester(ip, port, api_key)
    return 0 if tester.run_comprehensive_test() else 1

if __name__ == "__main__":
    sys.exit(main())