#!/usr/bin/env python3
"""
VPNHotspot简化远程连接测试工具
使用标准库测试192.168.1.133的连接
"""

import urllib.request
import urllib.error
import json
import subprocess
import sys
import socket

class SimpleRemoteTester:
    def __init__(self, ip="192.168.1.133", port=9999, api_key="default_api_key_for_debug_2024"):
        self.ip = ip
        self.port = port
        self.api_key = api_key
        self.base_url = f"http://{ip}:{port}/{api_key}"
    
    def check_port_open(self):
        """检查端口是否开放"""
        print("🔍 检查端口开放状态...")
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(3)
            result = sock.connect_ex((self.ip, self.port))
            sock.close()
            
            if result == 0:
                print("   ✅ 端口开放")
                return True
            else:
                print(f"   ❌ 端口 {self.port} 未开放")
                return False
        except Exception as e:
            print(f"   ❌ 检查端口时出错: {e}")
            return False
    
    def test_http_request(self):
        """测试HTTP请求"""
        print("🌐 测试HTTP连接...")
        try:
            url = f"{self.base_url}/api/status"
            print(f"   目标URL: {url}")
            
            req = urllib.request.Request(url)
            req.add_header('Accept', 'application/json')
            
            with urllib.request.urlopen(req, timeout=5) as response:
                data = response.read().decode('utf-8')
                status_code = response.getcode()
                
                print(f"   响应状态: {status_code}")
                
                if status_code == 200:
                    try:
                        json_data = json.loads(data)
                        print(f"   响应数据: {json.dumps(json_data, indent=2, ensure_ascii=False)}")
                        return True
                    except json.JSONDecodeError:
                        print(f"   响应内容: {data}")
                        return True
                else:
                    print(f"   错误响应: {data}")
                    return False
                    
        except urllib.error.HTTPError as e:
            print(f"   HTTP错误: {e.code} - {e.reason}")
            return False
        except urllib.error.URLError as e:
            print(f"   连接错误: {e.reason}")
            return False
        except socket.timeout:
            print("   ❌ 连接超时")
            return False
        except Exception as e:
            print(f"   ❌ 异常: {e}")
            return False
    
    def test_wifi_control(self, action="start"):
        """测试WiFi控制"""
        print(f"📡 测试WiFi {action}...")
        try:
            url = f"{self.base_url}/api/wifi/{action}"
            print(f"   目标URL: {url}")
            
            req = urllib.request.Request(url, method='POST')
            req.add_header('Accept', 'application/json')
            
            with urllib.request.urlopen(req, timeout=10) as response:
                data = response.read().decode('utf-8')
                status_code = response.getcode()
                
                print(f"   响应状态: {status_code}")
                
                if status_code == 200:
                    try:
                        json_data = json.loads(data)
                        print(f"   响应数据: {json.dumps(json_data, indent=2, ensure_ascii=False)}")
                        success = json_data.get('success', False)
                        if success:
                            print(f"   ✅ WiFi {action}成功")
                        else:
                            print(f"   ⚠️  WiFi {action}失败: {json_data.get('error', '未知错误')}")
                        return success
                    except json.JSONDecodeError:
                        print(f"   响应内容: {data}")
                        return False
                else:
                    print(f"   错误响应: {data}")
                    return False
                    
        except Exception as e:
            print(f"   ❌ 异常: {e}")
            return False
    
    def check_device_adb(self):
        """检查ADB设备"""
        print("🔍 检查ADB设备...")
        try:
            result = subprocess.run(['adb', 'devices'], 
                                  capture_output=True, text=True, timeout=5)
            
            if result.returncode != 0:
                print(f"   ❌ ADB错误: {result.stderr}")
                return False
            
            devices = [line for line in result.stdout.split('\n') 
                      if '\tdevice' in line and self.ip in line]
            
            if devices:
                print("   ✅ 设备已连接")
                return True
            else:
                print("   尝试连接设备...")
                result = subprocess.run(['adb', 'connect', f'{self.ip}:5555'], 
                                      capture_output=True, text=True, timeout=10)
                
                if "connected" in result.stdout.lower():
                    print("   ✅ 设备已连接")
                    return True
                else:
                    print(f"   ❌ 连接失败: {result.stdout}")
                    return False
                    
        except Exception as e:
            print(f"   ❌ ADB检查失败: {e}")
            return False
    
    def run_test(self):
        """运行测试"""
        print("🚀 VPNHotspot远程连接测试")
        print("=" * 50)
        print(f"目标: {self.ip}:{self.port}")
        print(f"API Key: {self.api_key}")
        print()
        
        # 检查ADB设备
        if not self.check_device_adb():
            print("\n❌ 请确保ADB设备已连接")
            return False
        
        # 检查端口
        if not self.check_port_open():
            print("\n❌ 端口未开放，请确保VPNHotspot应用已启动")
            return False
        
        # 测试基本连接
        if not self.test_http_request():
            print("\n❌ 基本连接测试失败")
            return False
        
        # 测试WiFi控制
        print("\n🔄 测试WiFi控制...")
        
        # 测试启动
        self.test_wifi_control("start")
        time.sleep(2)
        
        # 测试停止
        self.test_wifi_control("stop")
        
        print("\n" + "=" * 50)
        print("✅ 所有测试完成！")
        return True

def main():
    """主函数"""
    import time
    
    # 参数处理
    ip = "192.168.1.133"
    port = 9999
    api_key = "default_api_key_for_debug_2024"
    
    if len(sys.argv) > 1:
        ip = sys.argv[1]
    if len(sys.argv) > 2:
        port = int(sys.argv[2])
    if len(sys.argv) > 3:
        api_key = sys.argv[3]
    
    tester = SimpleRemoteTester(ip, port, api_key)
    return 0 if tester.run_test() else 1

if __name__ == "__main__":
    sys.exit(main())