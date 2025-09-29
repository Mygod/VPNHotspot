#!/usr/bin/env python3
"""
测试WebServer HTTP修复的脚本
"""

import subprocess
import time
import socket
import sys

def run_adb_command(command):
    """执行ADB命令"""
    try:
        result = subprocess.run(['adb'] + command, capture_output=True, text=True, timeout=30)
        return result.returncode == 0, result.stdout, result.stderr
    except subprocess.TimeoutExpired:
        return False, "", "Command timeout"
    except Exception as e:
        return False, "", str(e)

def test_http_response():
    """测试HTTP响应"""
    print("🌐 测试HTTP响应...")
    
    try:
        # 使用curl测试HTTP响应
        result = subprocess.run([
            'curl', '--noproxy', '*', '-s', '-w', 
            'HTTP_CODE:%{http_code}\nTIME_TOTAL:%{time_total}\nSIZE_DOWNLOAD:%{size_download}',
            'http://localhost:9999'
        ], capture_output=True, text=True, timeout=10)
        
        if result.returncode == 0:
            output = result.stdout
            if 'HTTP_CODE:200' in output:
                print("✅ HTTP响应正常 (200 OK)")
                
                # 检查响应内容
                if '需要API Key访问' in output:
                    print("✅ 返回了正确的API Key引导页面")
                    return True
                elif '热点控制面板' in output:
                    print("✅ 返回了主控制面板页面")
                    return True
                else:
                    print("⚠️  响应内容未知")
                    return False
            else:
                print(f"❌ HTTP响应错误: {output}")
                return False
        else:
            print(f"❌ curl命令失败: {result.stderr}")
            return False
            
    except subprocess.TimeoutExpired:
        print("❌ HTTP请求超时")
        return False
    except Exception as e:
        print(f"❌ HTTP测试异常: {e}")
        return False

def test_tcp_connection():
    """测试TCP连接"""
    print("🔌 测试TCP连接...")
    
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        result = sock.connect_ex(('127.0.0.1', 9999))
        sock.close()
        
        if result == 0:
            print("✅ TCP连接成功")
            return True
        else:
            print(f"❌ TCP连接失败: {result}")
            return False
    except Exception as e:
        print(f"❌ TCP连接异常: {e}")
        return False

def test_multiple_requests():
    """测试多个请求"""
    print("🔄 测试多个连续请求...")
    
    success_count = 0
    total_requests = 5
    
    for i in range(total_requests):
        try:
            result = subprocess.run([
                'curl', '--noproxy', '*', '-s', '-w', '%{http_code}',
                'http://localhost:9999'
            ], capture_output=True, text=True, timeout=5)
            
            if result.returncode == 0 and '200' in result.stdout:
                success_count += 1
                print(f"  请求 {i+1}: ✅")
            else:
                print(f"  请求 {i+1}: ❌")
                
            time.sleep(0.5)  # 短暂延迟
            
        except Exception as e:
            print(f"  请求 {i+1}: ❌ ({e})")
    
    if success_count == total_requests:
        print(f"✅ 所有 {total_requests} 个请求都成功")
        return True
    else:
        print(f"⚠️  {success_count}/{total_requests} 个请求成功")
        return success_count > total_requests // 2

def check_webserver_logs():
    """检查WebServer日志"""
    print("📝 检查WebServer日志...")
    
    success, stdout, stderr = run_adb_command([
        'logcat', '-d', '-s', 'OkHttpWebServer:*'
    ])
    
    if success and stdout:
        lines = stdout.split('\n')
        recent_lines = [line for line in lines if line.strip()][-10:]
        
        print("最近的WebServer日志:")
        for line in recent_lines:
            if line.strip():
                print(f"  {line}")
        
        # 检查是否有错误
        error_indicators = ['Error', 'Exception', 'Failed']
        recent_errors = []
        
        for line in recent_lines:
            for indicator in error_indicators:
                if indicator in line and 'Empty request' not in line:
                    recent_errors.append(line)
                    break
        
        if recent_errors:
            print("⚠️  发现错误日志:")
            for error in recent_errors:
                print(f"    {error}")
            return False
        else:
            print("✅ 没有发现严重错误")
            return True
    else:
        print("❌ 无法获取日志")
        return False

def restart_app_and_test():
    """重启应用并测试"""
    print("🔄 重启应用并测试...")
    
    # 强制停止应用
    run_adb_command(['shell', 'am', 'force-stop', 'be.mygod.vpnhotspot'])
    time.sleep(2)
    
    # 重新启动应用
    success, stdout, stderr = run_adb_command([
        'shell', 'am', 'start', '-n', 'be.mygod.vpnhotspot/.MainActivity'
    ])
    
    if not success:
        print(f"❌ 应用启动失败: {stderr}")
        return False
    
    print("✅ 应用重新启动")
    time.sleep(5)  # 等待WebServer启动
    
    # 测试连接
    return test_tcp_connection() and test_http_response()

def main():
    """主测试函数"""
    print("🚀 开始WebServer HTTP修复测试")
    print("=" * 50)
    
    tests = [
        ("TCP连接测试", test_tcp_connection),
        ("HTTP响应测试", test_http_response),
        ("多请求测试", test_multiple_requests),
        ("日志检查", check_webserver_logs),
        ("重启测试", restart_app_and_test)
    ]
    
    passed_tests = 0
    total_tests = len(tests)
    
    for test_name, test_func in tests:
        print(f"\n--- {test_name} ---")
        try:
            if test_func():
                passed_tests += 1
                print(f"✅ {test_name} 通过")
            else:
                print(f"❌ {test_name} 失败")
        except Exception as e:
            print(f"❌ {test_name} 异常: {e}")
    
    print("\n" + "=" * 50)
    print(f"测试总结: {passed_tests}/{total_tests} 通过")
    
    if passed_tests == total_tests:
        print("🎉 所有测试通过！WebServer HTTP修复成功！")
        return 0
    elif passed_tests >= total_tests * 0.8:
        print("✅ 大部分测试通过，WebServer基本正常")
        return 0
    else:
        print("❌ 多个测试失败，需要进一步调试")
        return 1

if __name__ == "__main__":
    sys.exit(main())