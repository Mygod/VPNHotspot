# VPNHotspot WebServer 测试套件

## 🧪 测试结构

### 🔧 unit/ - 单元测试
针对单个组件的测试脚本

- **test_okhttp_webserver_resource_management.py** - OkHttpWebServer资源管理测试
- **test_webserver_manager.py** - WebServerManager功能测试
- **verify_settings.py** - 设置验证测试

### 🔗 integration/ - 集成测试
多组件协作的集成测试

- **test_all_webserver_fixes.py** - 综合WebServer修复测试
- **test_api_key_workflow.py** - API Key工作流程测试
- **test_clipboard_error_handling.py** - 剪贴板错误处理测试
- **test_crash_fix.py** - 崩溃修复测试
- **test_webserver_http_fix.py** - HTTP修复测试
- **test_auto_connect.sh** - 自动连接测试脚本
- **test_webserver.sh** - WebServer测试脚本

### 📱 device/ - 设备测试
在真实设备上运行的测试

- **test_device_functionality.py** - 设备功能综合测试
- **test_connection_simple.py** - 简单连接测试
- **test_remote_connection.py** - 远程连接测试

## 🚀 运行测试

### 前置条件
- Python 3.x
- ADB工具
- 已连接的Android设备
- 已安装的VPNHotspot APK

### 运行单元测试
```bash
cd tests/unit
python3 test_webserver_manager.py
python3 test_okhttp_webserver_resource_management.py
python3 verify_settings.py
```

### 运行集成测试
```bash
cd tests/integration
python3 test_all_webserver_fixes.py
python3 test_api_key_workflow.py
python3 test_clipboard_error_handling.py
python3 test_crash_fix.py
python3 test_webserver_http_fix.py
```

### 运行设备测试
```bash
cd tests/device
python3 test_device_functionality.py
python3 test_connection_simple.py
python3 test_remote_connection.py
```

### 运行Shell测试
```bash
cd tests/integration
bash test_auto_connect.sh
bash test_webserver.sh
```

## 📊 测试报告

测试运行后会生成相应的报告文件，存放在各自的测试目录中。主要报告会自动移动到 `../docs/reports/` 目录。

## 🔍 测试覆盖

### 功能测试
- ✅ WebServer启动和停止
- ✅ 端口冲突处理
- ✅ API Key认证
- ✅ HTTP请求处理
- ✅ 剪贴板操作
- ✅ 应用生命周期

### 稳定性测试
- ✅ 多次重启测试
- ✅ 长时间运行测试
- ✅ 并发请求测试
- ✅ 异常恢复测试

### 设备测试
- ✅ 真实设备功能验证
- ✅ 网络连接测试
- ✅ UI交互测试
- ✅ 性能监控

## 🐛 故障排除

### 常见问题
1. **ADB连接失败**: 确保设备已连接且开启USB调试
2. **端口占用**: 检查9999端口是否被其他应用占用
3. **权限问题**: 确保应用有必要的网络和存储权限
4. **Python依赖**: 某些测试可能需要额外的Python包

### 调试技巧
- 使用 `adb logcat` 查看实时日志
- 检查 `adb devices` 确认设备连接
- 运行单个测试文件进行问题定位

## 📈 测试结果解读

### 成功标识
- ✅ 测试通过
- 🎉 所有测试通过

### 警告标识
- ⚠️ 部分功能异常但不影响主要功能
- 💡 建议或提示信息

### 失败标识
- ❌ 测试失败
- 🔥 严重错误需要立即修复

## 🔄 持续集成

建议将这些测试集成到CI/CD流程中，确保每次代码变更都经过完整测试验证。